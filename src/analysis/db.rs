//! SQLite storage for track features, acoustic metrics, and mix vectors.
//! Strictly isolated inside ~/.cache/quickstream/track_analysis.db (never touches user library).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};

/// Current database schema version (tracked via PRAGMA user_version).
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Open or create SQLite database at a specific path.
    pub fn open_at(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Run automatic schema migrations using PRAGMA user_version.
    fn run_migrations(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();

        // 1. Check existing schema version
        let user_version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        if user_version < 1 {
            // Version 1 Migration: Initial track_features table and indexes
            conn.execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;

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

                PRAGMA user_version = 1;
                "#,
            )?;
        }

        Ok(())
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

        let mut f = TrackFeatures::default();
        f.filepath = PathBuf::from("/music/electronic/track1.flac");
        f.title = "Strobe".to_string();
        f.artist = "deadmau5".to_string();
        f.bpm = Some(128.0);
        f.initial_key = Some("Am".to_string());
        f.camelot_key = Some("8A".to_string());
        f.energy = Some(0.85);
        f.mood = Some("workout".to_string());

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
}
