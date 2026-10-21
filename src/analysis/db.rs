//! SQLite storage for track features, acoustic metrics, and mix vectors.
//! Strictly isolated inside ~/.cache/quickstream/track_analysis.db (never touches user library).

use rusqlite::{params, Connection, Result, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Current database schema version (tracked via PRAGMA user_version).
/// Always equal to `MIGRATIONS.len()` (checked at compile time below).
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

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
    // v2 (#41): library index. `needs_acoustic = 1` marks a tag-only row (no BPM tag) that still
    // needs the ffmpeg pass; every v1 row was written by the full analyser and reads 0. Both
    // statements are O(1): no index is built and no row is rewritten.
    r#"
    ALTER TABLE track_features ADD COLUMN needs_acoustic INTEGER NOT NULL DEFAULT 0;

    CREATE TABLE IF NOT EXISTS library_index_dirs (
        dirpath TEXT PRIMARY KEY,
        audio_files INTEGER NOT NULL,
        complete INTEGER NOT NULL DEFAULT 0,
        subtree_complete INTEGER NOT NULL DEFAULT 0,
        walked_at INTEGER NOT NULL
    );
    "#,
    // v3 (#41 review): folder freshness and failed files. `dir_mtime` is the folder's own mtime
    // (nanoseconds since the epoch, as the share reports it) seen when it was listed for the
    // subtree mark it goes with: set together with `subtree_complete` once the whole subtree has
    // been walked, so a mix compares it for equality with the folder's mtime now and no clock is
    // involved. NULL (every v2 record) is never trusted. `library_index_failures` remembers the
    // files whose stat or tag read failed (or panicked), per stamp (-1, -1 for a failed stat),
    // so a file that always fails stops keeping its folder incomplete from its second pass on.
    r#"
    ALTER TABLE library_index_dirs ADD COLUMN dir_mtime INTEGER;

    CREATE TABLE IF NOT EXISTS library_index_failures (
        filepath TEXT PRIMARY KEY,
        mtime INTEGER NOT NULL,
        file_size INTEGER NOT NULL,
        panicked INTEGER NOT NULL DEFAULT 0,
        failed_at INTEGER NOT NULL
    );
    "#,
];

const _: () = assert!(MIGRATIONS.len() as u32 == CURRENT_SCHEMA_VERSION);

/// Returned when the database on disk was written by a newer quickstream build.
#[derive(Debug)]
pub struct SchemaTooNewError {
    pub path: String,
    pub found: u32,
    pub supported: u32,
}

impl std::fmt::Display for SchemaTooNewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "analysis database {} has schema version {}, which is newer than this build supports \
             (version {}); refusing to open it so it is not damaged. Upgrade quickstream, or move \
             the file aside to rebuild the analysis cache",
            self.path, self.found, self.supported
        )
    }
}

impl std::error::Error for SchemaTooNewError {}

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
    pub energy: Option<f64>, // 0.0 (very low energy / ambient) to 1.0 (very energetic / peak)
    pub loudness_lufs: Option<f64>, // Integrated LUFS
    pub mood: Option<String>, // "chill", "workout", "focus", "groove"
    pub tags_json: String,   // JSON array of community / style tags
    pub analyzed_at: u64,
    /// A tag-only row from the library index (#41) with no BPM tag: it still needs the acoustic
    /// pass, so it is not current to the worker ([`row_is_current`]) whatever its stamp. Every
    /// full analysis is stored with this cleared.
    ///
    /// [`row_is_current`]: crate::analysis::worker::row_is_current
    #[serde(default)]
    pub needs_acoustic: bool,
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
            needs_acoustic: false,
        }
    }
}

/// A folder the library index has walked: how many audio files sit directly inside it, and
/// whether every one of them has a current row (`complete`). Written by
/// [`AnalysisDatabase::write_index_batch`] together with the folder's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDir {
    pub dirpath: PathBuf,
    pub audio_files: usize,
    pub complete: bool,
    pub walked_at: u64,
}

/// A file the library index couldn't index: the stamp (mtime, size) it failed for, `None` when
/// the file couldn't even be stat'ed, and whether its tag read panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexFailure {
    pub stamp: Option<(u64, u64)>,
    pub panicked: bool,
}

/// A modification time as the library index stores a folder's (`dir_mtime`): nanoseconds since
/// the epoch, negative before it; `None` outside the range an i64 holds.
pub fn mtime_nanos(t: std::time::SystemTime) -> Option<i64> {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).ok(),
        Err(before) => i64::try_from(before.duration().as_nanos()).ok().map(|n| -n),
    }
}

/// The columns [`row_to_features`] reads, in its order.
const FEATURE_COLUMNS: &str = "filepath, mtime, file_size, title, artist, album, genre, \
     year, bpm, initial_key, camelot_key, energy, loudness_lufs, \
     mood, tags_json, analyzed_at, needs_acoustic";

/// One row selected with [`FEATURE_COLUMNS`]. Any non-zero `needs_acoustic` reads as set, so an
/// odd value leads to a re-analysis rather than a skip.
fn row_to_features(row: &Row<'_>) -> Result<TrackFeatures> {
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
        needs_acoustic: row.get::<_, i64>(16)? != 0,
    })
}

/// How a folder is keyed in `library_index_dirs` and bounds `tracks_under`: the lossy string
/// form with no trailing `/` (the root stays `/`).
fn dir_key(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() && s.starts_with('/') {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Whether the library walks skip an entry named `name` (and everything under it): hidden names
/// (`.` first, AppleDouble `._` files included), `#recycle` and `$RECYCLE.BIN`. The autopilot
/// walk filters with it, and [`AnalysisDatabase::tracks_under`] applies it to index rows, so the
/// index never offers a file the walk would not find.
pub fn walk_skips(name: &str) -> bool {
    name.starts_with('.') || name == "#recycle" || name == "$RECYCLE.BIN"
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
    /// The file it was opened at ([`Self::open_at`]); `None` in memory.
    path: Option<PathBuf>,
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
        Self::from_connection(Connection::open_in_memory()?, ":memory:", None)
    }

    /// Open or create SQLite database at a specific path.
    ///
    /// Fails with [`SchemaTooNewError`] (leaving the file untouched) if the database was written
    /// by a newer quickstream build.
    pub fn open_at(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn, &path.display().to_string(), Some(path.to_path_buf()))
    }

    /// The file this database was opened at, so another thread can open its own connection to
    /// it; `None` for an in-memory database.
    pub fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    /// Configure a freshly opened connection and bring its schema up to date.
    fn from_connection(mut conn: Connection, label: &str, path: Option<PathBuf>) -> anyhow::Result<Self> {
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
            path,
        })
    }

    /// Upsert a full analysis of a track. It always clears `needs_acoustic`, so it replaces a
    /// tag-only index row whatever the struct carries.
    pub fn upsert_track(&self, features: &TrackFeatures) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO track_features (
                filepath, mtime, file_size, title, artist, album, genre,
                year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                mood, tags_json, analyzed_at, needs_acoustic
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 0
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
                analyzed_at = excluded.analyzed_at,
                needs_acoustic = 0;
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

    /// Write the library index's tag-only rows (and, with `dir`, that folder's record) in one
    /// IMMEDIATE transaction, so a folder is never recorded without its rows. Returns how many
    /// rows were inserted or changed.
    ///
    /// A row already current for the file's stamp (mtime and size) is never touched, whoever
    /// wrote it. Over a stale row the tags, stamp and `needs_acoustic` are replaced; a missing
    /// BPM tag keeps the old BPM and energy provisionally (the row is flagged for re-analysis),
    /// while a BPM tag replaces the BPM and drops the energy measured for the old file. Loudness
    /// and mood, which a tag read doesn't give, are cleared (they were measured or looked up for
    /// the old file), and `tags_json` is the new row's. Unknown values are stored as NULL.
    pub fn write_index_batch(&self, rows: &[TrackFeatures], dir: Option<&IndexedDir>) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO track_features (
                    filepath, mtime, file_size, title, artist, album, genre,
                    year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                    mood, tags_json, analyzed_at, needs_acoustic
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL, NULL, ?14, ?12, ?13
                )
                ON CONFLICT(filepath) DO UPDATE SET
                    mtime = excluded.mtime,
                    file_size = excluded.file_size,
                    title = excluded.title,
                    artist = excluded.artist,
                    album = excluded.album,
                    genre = excluded.genre,
                    year = excluded.year,
                    initial_key = excluded.initial_key,
                    camelot_key = excluded.camelot_key,
                    bpm = COALESCE(excluded.bpm, track_features.bpm),
                    energy = CASE WHEN excluded.bpm IS NULL THEN track_features.energy ELSE NULL END,
                    loudness_lufs = NULL,
                    mood = NULL,
                    tags_json = excluded.tags_json,
                    needs_acoustic = excluded.needs_acoustic,
                    analyzed_at = excluded.analyzed_at
                WHERE track_features.mtime IS NOT excluded.mtime
                   OR track_features.file_size IS NOT excluded.file_size;
                "#,
            )?;
            for row in rows {
                written += stmt.execute(params![
                    row.filepath.to_string_lossy().to_string(),
                    row.mtime as i64,
                    row.file_size as i64,
                    row.title,
                    row.artist,
                    row.album,
                    row.genre,
                    row.year,
                    row.bpm,
                    row.initial_key,
                    row.camelot_key,
                    row.analyzed_at as i64,
                    row.needs_acoustic as i64,
                    row.tags_json,
                ])?;
            }
        }
        if !rows.is_empty() {
            // A file read now is no longer a failure.
            let mut stmt = tx.prepare_cached("DELETE FROM library_index_failures WHERE filepath = ?1")?;
            for row in rows {
                stmt.execute(params![row.filepath.to_string_lossy().to_string()])?;
            }
        }
        if let Some(dir) = dir {
            // The subtree mark and the folder mtime it was set for are left as they are: only
            // `set_subtree_complete` changes them, once the walk below the folder is done again.
            // A re-listing doesn't refresh them early, and doesn't take them away either: a mix
            // trusts the mark only while the folder's mtime still equals `dir_mtime`.
            tx.execute(
                r#"
                INSERT INTO library_index_dirs (dirpath, audio_files, complete, subtree_complete, walked_at)
                VALUES (?1, ?2, ?3, 0, ?4)
                ON CONFLICT(dirpath) DO UPDATE SET
                    audio_files = excluded.audio_files,
                    complete = excluded.complete,
                    walked_at = excluded.walked_at;
                "#,
                params![
                    dir_key(&dir.dirpath),
                    dir.audio_files as i64,
                    dir.complete as i64,
                    dir.walked_at as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }

    /// The stamps (mtime, size) of the rows these files have, keyed by the lossy path string;
    /// files without a row are left out.
    pub fn index_stamps(&self, paths: &[PathBuf]) -> HashMap<String, (u64, u64)> {
        let conn = self.conn.lock().unwrap();
        let mut out = HashMap::new();
        let Ok(mut stmt) = conn.prepare_cached("SELECT mtime, file_size FROM track_features WHERE filepath = ?1")
        else {
            return out;
        };
        for path in paths {
            let key = path.to_string_lossy().to_string();
            let stamp = stmt.query_row(params![key], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
            });
            if let Ok(stamp) = stamp {
                out.insert(key, stamp);
            }
        }
        out
    }

    /// Record whether `dir` and every folder under it have all their rows: `Some(mtime)` when the
    /// whole subtree was walked, `mtime` being the folder's own mtime ([`mtime_nanos`]) as seen
    /// when it was listed for that walk; `None` takes the mark away. Returns false when `dir` has
    /// no record (none is created).
    pub fn set_subtree_complete(&self, dir: &Path, whole_at: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE library_index_dirs SET subtree_complete = ?2, dir_mtime = ?3 WHERE dirpath = ?1",
            params![dir_key(dir), whole_at.is_some() as i64, whole_at],
        )?;
        Ok(changed > 0)
    }

    /// The folders among `dirs` whose whole subtree is indexed, each with the folder mtime its
    /// mark was set for ([`Self::set_subtree_complete`]). A reader trusts the mark only while the
    /// folder's mtime is still exactly that (`!=`, never `>`: the share's clock is not ours).
    pub fn indexed_subtrees(&self, dirs: &[PathBuf]) -> HashMap<PathBuf, i64> {
        let conn = self.conn.lock().unwrap();
        let mut out = HashMap::new();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT dir_mtime FROM library_index_dirs \
             WHERE dirpath = ?1 AND subtree_complete != 0 AND dir_mtime IS NOT NULL",
        ) else {
            return out;
        };
        for dir in dirs {
            if let Ok(mtime) = stmt.query_row(params![dir_key(dir)], |r| r.get::<_, i64>(0)) {
                out.insert(dir.clone(), mtime);
            }
        }
        out
    }

    /// The failures recorded for these files ([`Self::record_index_failure`]), keyed by the lossy
    /// path string; files without one are left out.
    pub fn index_failures(&self, paths: &[PathBuf]) -> HashMap<String, IndexFailure> {
        let conn = self.conn.lock().unwrap();
        let mut out = HashMap::new();
        let Ok(mut stmt) =
            conn.prepare_cached("SELECT mtime, file_size, panicked FROM library_index_failures WHERE filepath = ?1")
        else {
            return out;
        };
        for path in paths {
            let key = path.to_string_lossy().to_string();
            let failure = stmt.query_row(params![key], |r| {
                let (mtime, size) = (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
                Ok(IndexFailure {
                    stamp: (mtime >= 0 && size >= 0).then_some((mtime as u64, size as u64)),
                    panicked: r.get::<_, i64>(2)? != 0,
                })
            });
            if let Ok(failure) = failure {
                out.insert(key, failure);
            }
        }
        out
    }

    /// Remember that the library index couldn't stat `path` (`stamp` `None`) or read its tags for
    /// `stamp` (`panicked` when the read panicked), replacing an earlier failure of it.
    pub fn record_index_failure(&self, path: &Path, failure: IndexFailure, failed_at: u64) -> Result<()> {
        let (mtime, size) = match failure.stamp {
            Some((mtime, size)) => (mtime as i64, size as i64),
            None => (-1, -1),
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO library_index_failures (filepath, mtime, file_size, panicked, failed_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(filepath) DO UPDATE SET
                mtime = excluded.mtime,
                file_size = excluded.file_size,
                panicked = excluded.panicked,
                failed_at = excluded.failed_at;
            "#,
            params![
                path.to_string_lossy().to_string(),
                mtime,
                size,
                failure.panicked as i64,
                failed_at as i64
            ],
        )?;
        Ok(())
    }

    /// A folder's record, and whether its whole subtree is indexed.
    pub fn index_dir(&self, dir: &Path) -> Option<(IndexedDir, bool)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT dirpath, audio_files, complete, walked_at, subtree_complete \
             FROM library_index_dirs WHERE dirpath = ?1",
            params![dir_key(dir)],
            |r| {
                Ok((
                    IndexedDir {
                        dirpath: PathBuf::from(r.get::<_, String>(0)?),
                        audio_files: r.get::<_, i64>(1)? as usize,
                        complete: r.get::<_, i64>(2)? != 0,
                        walked_at: r.get::<_, i64>(3)? as u64,
                    },
                    r.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .ok()
    }

    /// The rows of files under `dir`, at most `max_depth` levels down (1 = directly inside, as
    /// `WalkDir::max_depth` counts). A byte range on the primary key (`dir/` up to `dir0`, `0`
    /// being the byte after `/`), never LIKE, so a sibling that shares the name as a prefix
    /// (`Jazz Fusion` for `Jazz`) is never included. A row with a name below `dir` that the
    /// mix's walk skips ([`walk_skips`]: hidden, `#recycle`, `$RECYCLE.BIN`) is left out too.
    pub fn tracks_under(&self, dir: &Path, max_depth: usize) -> Vec<TrackFeatures> {
        let key = dir_key(dir);
        let base = key.trim_end_matches('/');
        let (low, high) = (format!("{base}/"), format!("{base}0"));
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {FEATURE_COLUMNS} FROM track_features WHERE filepath >= ?1 AND filepath < ?2");
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(params![low, high], row_to_features) else {
            return Vec::new();
        };
        rows.filter_map(|r| r.ok())
            .filter(|t| {
                let path = t.filepath.to_string_lossy();
                let Some(rest) = path.get(low.len()..) else {
                    return false;
                };
                let mut depth = 0;
                for name in rest.split('/').filter(|c| !c.is_empty()) {
                    if walk_skips(name) {
                        return false;
                    }
                    depth += 1;
                }
                depth <= max_depth
            })
            .collect()
    }

    /// Retrieve track features by filepath.
    pub fn get_features(&self, path: &Path) -> Option<TrackFeatures> {
        self.get_track(path)
    }

    /// Retrieve track features by filepath.
    pub fn get_track(&self, path: &Path) -> Option<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let path_str = path.to_string_lossy().to_string();
        let sql = format!("SELECT {FEATURE_COLUMNS} FROM track_features WHERE filepath = ?1");
        let mut stmt = conn.prepare(&sql).ok()?;
        stmt.query_row(params![path_str], row_to_features).ok()
    }

    /// Query tracks by tempo / BPM window.
    pub fn query_bpm_range(&self, min_bpm: f64, max_bpm: f64, limit: usize) -> Vec<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {FEATURE_COLUMNS} FROM track_features \
             WHERE bpm IS NOT NULL AND bpm >= ?1 AND bpm <= ?2 ORDER BY RANDOM() LIMIT ?3"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![min_bpm, max_bpm, limit as i64], row_to_features) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Query tracks by mood (e.g. "chill", "workout", "focus", "groove").
    pub fn query_by_mood(&self, mood: &str, limit: usize) -> Vec<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {FEATURE_COLUMNS} FROM track_features \
             WHERE mood = ?1 OR tags_json LIKE ?2 ORDER BY RANDOM() LIMIT ?3"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let tag_pattern = format!("%{}%", mood);
        let rows = match stmt.query_map(params![mood, tag_pattern, limit as i64], row_to_features) {
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
        assert_eq!(
            pragma_i64(&reopened, "synchronous"),
            1,
            "synchronous must be NORMAL (1)"
        );
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
        let fixed: &[&str] = &[
            "CREATE TABLE step_one (x INTEGER);",
            "CREATE TABLE step_two (y INTEGER);",
        ];
        apply_migrations(&mut conn, fixed, ":memory:").unwrap();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        assert_eq!(count(&conn, "step_two"), 1);
    }
}
