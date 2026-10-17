//! Intelligent Mix Engine: sequences tracks into cohesive mixes using tempo, harmonic keys, and mood.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use super::db::{AnalysisDatabase, TrackFeatures};
use super::harmonic::{CamelotKey, MusicalKey};
use crate::metadata::AudioMetadata;

/// After a real artist plays, this many following tracks must be by other artists.
pub const ARTIST_COOLDOWN: usize = 8;
/// After an album plays, this many following tracks must be from other albums.
pub const ALBUM_COOLDOWN: usize = 12;

/// Camelot key stored in a track's features, if any.
fn camelot_of(feat: Option<&TrackFeatures>) -> Option<CamelotKey> {
    feat.and_then(|f| f.camelot_key.as_deref())
        .and_then(MusicalKey::parse_to_camelot)
}

/// Mood presets for intelligent vibe mixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoodType {
    Chill,   // Ambient, downtempo, acoustic, classical, lofi (60-95 BPM, low energy)
    Workout, // Electronic, rock, dance, hip-hop (125-140 BPM, high energy)
    Focus,   // Instrumental, synthwave, minimalist, piano
    Groove,  // Funk, soul, disco, house (115-128 BPM)
}

impl MoodType {
    pub fn name(&self) -> &'static str {
        match self {
            MoodType::Chill => "Chill & Ambient",
            MoodType::Workout => "Workout & High Energy",
            MoodType::Focus => "Focus & Instrumental",
            MoodType::Groove => "Late Night Groove",
        }
    }

    pub fn bpm_range(&self) -> (f64, f64) {
        match self {
            MoodType::Chill => (60.0, 98.0),
            MoodType::Workout => (124.0, 145.0),
            MoodType::Focus => (70.0, 115.0),
            MoodType::Groove => (110.0, 128.0),
        }
    }

    pub fn matches_features(&self, feat: &TrackFeatures) -> bool {
        let meta = AudioMetadata {
            filepath: feat.filepath.clone(),
            title: feat.title.clone(),
            artist: feat.artist.clone(),
            album: feat.album.clone(),
            genre: feat.genre.clone(),
            ..Default::default()
        };
        self.matches_track(&meta, Some(feat))
    }

    pub fn matches_track(&self, meta: &AudioMetadata, feat: Option<&TrackFeatures>) -> bool {
        let (min_bpm, max_bpm) = self.bpm_range();
        let feat_bpm = feat.and_then(|f| f.bpm);
        let bpm_ok = if let Some(b) = feat_bpm {
            (b >= min_bpm && b <= max_bpm)
                || (b >= min_bpm * 0.5 && b <= max_bpm * 0.5)
                || (b >= min_bpm * 2.0 && b <= max_bpm * 2.0)
        } else {
            true
        };

        let tags_lower = feat.map(|f| f.tags_json.to_lowercase()).unwrap_or_default();
        let feat_genre = feat.map(|f| f.genre.to_lowercase()).unwrap_or_default();
        let meta_genre = meta.genre.to_lowercase();
        let path_lower = meta.filepath.to_string_lossy().to_lowercase();
        let album_lower = meta.album.to_lowercase();
        let title_lower = meta.title.to_lowercase();
        let artist_lower = meta.artist.to_lowercase();

        let energy = feat.and_then(|f| f.energy);

        let has_term = |terms: &[&str]| -> bool {
            for &term in terms {
                if meta_genre.contains(term)
                    || feat_genre.contains(term)
                    || tags_lower.contains(term)
                    || path_lower.contains(term)
                    || album_lower.contains(term)
                    || title_lower.contains(term)
                    || artist_lower.contains(term)
                {
                    return true;
                }
            }
            false
        };

        match self {
            MoodType::Chill => {
                let chill_terms = [
                    "chill",
                    "ambient",
                    "downtempo",
                    "lofi",
                    "lo-fi",
                    "acoustic",
                    "classical",
                    "meditation",
                    "sleep",
                    "drone",
                    "trip hop",
                    "trip-hop",
                    "idm",
                    "easy listening",
                    "new age",
                    "relax",
                    "mellow",
                    "piano",
                    "peaceful",
                    "soft",
                    "calm",
                    "zen",
                ];
                let energy_ok = energy.map(|e| e <= 0.50).unwrap_or(false);
                bpm_ok && (has_term(&chill_terms) || energy_ok)
            }
            MoodType::Workout => {
                let workout_terms = [
                    "dance",
                    "electronic",
                    "rock",
                    "house",
                    "techno",
                    "upbeat",
                    "workout",
                    "fitness",
                    "metal",
                    "drum and bass",
                    "dnb",
                    "trance",
                    "edm",
                    "hip hop",
                    "hip-hop",
                    "rap",
                    "punk",
                    "hardcore",
                    "club",
                    "gym",
                    "fast",
                    "party",
                ];
                let energy_ok = energy.map(|e| e >= 0.65).unwrap_or(false);
                bpm_ok && (has_term(&workout_terms) || energy_ok)
            }
            MoodType::Focus => {
                let focus_terms = [
                    "instrumental",
                    "piano",
                    "soundtrack",
                    "synthwave",
                    "minimal",
                    "minimalist",
                    "ambient",
                    "classical",
                    "study",
                    "post-rock",
                    "score",
                    "film score",
                    "neo-classical",
                    "electronic",
                    "meditation",
                ];
                let energy_ok = energy.map(|e| e <= 0.60).unwrap_or(false);
                has_term(&focus_terms) || energy_ok
            }
            MoodType::Groove => {
                let groove_terms = [
                    "funk",
                    "soul",
                    "disco",
                    "groove",
                    "r&b",
                    "rhythm and blues",
                    "house",
                    "boogie",
                    "nu-disco",
                    "motown",
                    "afrobeat",
                    "jazz-funk",
                    "acid jazz",
                    "dance",
                    "funky",
                ];
                bpm_ok && has_term(&groove_terms)
            }
        }
    }
}

/// Profiles for Autopilot playlist generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum MixProfile {
    /// Fair uniform random sampling across diverse artists.
    #[default]
    DiverseShuffle,
    /// Cohesive BPM cluster (e.g. 124 BPM ± 5%).
    BpmFlow { target_bpm: f64, tolerance: f64 },
    /// Mood and vibe clustering.
    MoodVibe(MoodType),
    /// Radio station seeded by a chosen track or artist.
    SeedRadio(PathBuf),
}

impl MixProfile {
    pub fn name(&self) -> String {
        match self {
            MixProfile::DiverseShuffle => "Diverse Artists Shuffle".to_string(),
            MixProfile::BpmFlow { target_bpm, .. } => format!("BPM Flow (~{:.0} BPM)", target_bpm),
            MixProfile::MoodVibe(mood) => format!("Mood: {}", mood.name()),
            MixProfile::SeedRadio(path) => {
                let name = path.file_stem().unwrap_or_default().to_string_lossy();
                format!("Seed Radio ({})", name)
            }
        }
    }
}

/// DJ sequencing engine that applies artist separation and tempo continuity.
pub struct DjMixer;

impl DjMixer {
    /// Apply DJ spacing and smooth transition ordering to a pool of candidate tracks.
    pub fn sequence_mix(
        candidates: Vec<AudioMetadata>,
        features_db: &AnalysisDatabase,
        target_limit: usize,
    ) -> Vec<AudioMetadata> {
        Self::sequence_mix_after(None, candidates, features_db, target_limit)
    }

    /// Sequence `candidates` to follow `already_queued` (a track already sent to the queue), so
    /// artist/album spacing and BPM/key transitions account for it. The returned list does not
    /// include `already_queued`.
    pub fn sequence_mix_after(
        already_queued: Option<&AudioMetadata>,
        candidates: Vec<AudioMetadata>,
        features_db: &AnalysisDatabase,
        target_limit: usize,
    ) -> Vec<AudioMetadata> {
        if candidates.is_empty() {
            return Vec::new();
        }

        // 1. Look up each candidate's features (and parse its Camelot key) exactly once.
        let mut pool: Vec<Candidate> = candidates
            .into_iter()
            .map(|meta| {
                let feat = features_db.get_track(&meta.filepath);
                Candidate {
                    artist_key: artist_key(&meta.artist),
                    album_key: album_key(&meta.album),
                    bpm: feat.as_ref().and_then(|f| f.bpm),
                    camelot: camelot_of(feat.as_ref()),
                    meta,
                }
            })
            .collect();

        // 2. Greedy sequencing. Cooldowns are tracked by queue position (every track, tagged or
        //    not, occupies a position; the already-queued track is position 0).
        //    Selection order for each slot:
        //    a. artist cooldown (hard): a candidate whose real artist played within the last
        //       ARTIST_COOLDOWN positions is only chosen if no candidate satisfies the rule, and
        //       then the one whose artist played longest ago;
        //    b. album cooldown (hard, after the artist rule), same scheme with ALBUM_COOLDOWN;
        //    c. transition score: BPM continuity + Camelot compatibility with the previous track.
        //    Ties keep candidate order.
        let mut position = 0usize;
        let mut last_artist_pos: HashMap<String, usize> = HashMap::new();
        let mut last_album_pos: HashMap<String, usize> = HashMap::new();
        // Features of the most recently sequenced track, carried forward from the chosen
        // candidate instead of being re-queried from SQLite on every iteration.
        let mut last_bpm: Option<f64> = None;
        let mut last_camelot: Option<CamelotKey> = None;

        if let Some(first) = already_queued {
            if let Some(k) = artist_key(&first.artist) {
                last_artist_pos.insert(k, position);
            }
            if let Some(k) = album_key(&first.album) {
                last_album_pos.insert(k, position);
            }
            let feat = features_db.get_track(&first.filepath);
            last_bpm = feat.as_ref().and_then(|f| f.bpm);
            last_camelot = camelot_of(feat.as_ref());
            position += 1;
        }

        let mut sequenced: Vec<AudioMetadata> = Vec::with_capacity(target_limit.min(pool.len()));
        while !pool.is_empty() && sequenced.len() < target_limit {
            let shortfall = |key: &Option<String>, last: &HashMap<String, usize>, cooldown: usize| -> usize {
                key.as_ref()
                    .and_then(|k| last.get(k))
                    .map(|&prev| (cooldown + 1).saturating_sub(position - prev))
                    .unwrap_or(0)
            };

            let best_idx = pool
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| {
                    (
                        shortfall(&c.artist_key, &last_artist_pos, ARTIST_COOLDOWN),
                        shortfall(&c.album_key, &last_album_pos, ALBUM_COOLDOWN),
                        std::cmp::Reverse(transition_score(c, last_bpm, last_camelot)),
                    )
                })
                .map(|(idx, _)| idx)
                .unwrap_or(0);

            let chosen = pool.remove(best_idx);
            if let Some(k) = chosen.artist_key {
                last_artist_pos.insert(k, position);
            }
            if let Some(k) = chosen.album_key {
                last_album_pos.insert(k, position);
            }
            last_bpm = chosen.bpm;
            last_camelot = chosen.camelot;
            position += 1;
            sequenced.push(chosen.meta);
        }

        sequenced
    }
}

/// A candidate track with everything the sequencer needs precomputed.
struct Candidate {
    meta: AudioMetadata,
    artist_key: Option<String>,
    album_key: Option<String>,
    bpm: Option<f64>,
    camelot: Option<CamelotKey>,
}

/// Artists exempt from the artist cooldown (compilations and untagged tracks).
fn is_generic_artist(artist: &str) -> bool {
    matches!(
        artist.trim().to_lowercase().as_str(),
        "" | "various artists" | "various" | "unknown artist" | "unknown"
    )
}

/// Normalized artist identity for cooldown tracking; `None` for generic / untagged artists.
fn artist_key(artist: &str) -> Option<String> {
    (!is_generic_artist(artist)).then(|| artist.trim().to_lowercase())
}

/// Normalized album identity for cooldown tracking; `None` for untagged albums.
fn album_key(album: &str) -> Option<String> {
    let key = album.trim().to_lowercase();
    (!key.is_empty() && key != "unknown album").then_some(key)
}

/// Transition quality from the previous track: BPM continuity plus Camelot compatibility.
fn transition_score(c: &Candidate, last_bpm: Option<f64>, last_camelot: Option<CamelotKey>) -> i64 {
    let mut score: i64 = 100;
    if let (Some(cur_bpm), Some(prev_bpm)) = (c.bpm, last_bpm) {
        let diff = (cur_bpm - prev_bpm).abs();
        if diff <= 4.0 {
            score += 50; // Smooth blend
        } else if diff <= 8.0 {
            score += 20;
        } else {
            score -= (diff as i64) * 3;
        }
    }
    if let (Some(cur_cam), Some(prev_cam)) = (c.camelot, last_camelot) {
        score += prev_cam.compatibility_score(&cur_cam) as i64 / 2;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(dir: &str, artist: &str, i: usize) -> AudioMetadata {
        AudioMetadata {
            filepath: PathBuf::from(format!("/music/{}/{}.mp3", dir, i)),
            artist: artist.to_string(),
            title: format!("Song {}{}", dir, i),
            ..Default::default()
        }
    }

    fn store_bpm(db: &AnalysisDatabase, meta: &AudioMetadata, bpm: f64) {
        db.upsert_track(&TrackFeatures {
            filepath: meta.filepath.clone(),
            artist: meta.artist.clone(),
            bpm: Some(bpm),
            ..Default::default()
        })
        .unwrap();
    }

    /// BPM continuity follows the features of whatever was sequenced last, including the
    /// already-queued track (whose features are looked up once, not on every iteration).
    #[test]
    fn test_dj_mixer_follows_bpm_of_previous_track() {
        let db = AnalysisDatabase::open_in_memory().unwrap();
        let queued = song("Q", "Queued", 0);
        store_bpm(&db, &queued, 128.0);

        let far = song("F", "Far", 1);
        let near = song("N", "Near", 1);
        let nearer_to_near = song("M", "Mid", 1);
        store_bpm(&db, &far, 90.0);
        store_bpm(&db, &near, 127.0);
        store_bpm(&db, &nearer_to_near, 125.0);

        let mix = DjMixer::sequence_mix_after(Some(&queued), vec![far, near, nearer_to_near], &db, 3);
        let order: Vec<&str> = mix.iter().map(|m| m.artist.as_str()).collect();
        assert_eq!(order, vec!["Near", "Mid", "Far"]);
    }

    #[test]
    fn test_dj_mixer_spacing_rules() {
        let db = AnalysisDatabase::open_in_memory().unwrap();
        let mut candidates = Vec::new();

        // 5 songs by Artist A, 5 songs by Artist B, 5 songs by Artist C
        for i in 1..=5 {
            candidates.push(song("A", "Artist A", i));
            candidates.push(song("B", "Artist B", i));
            candidates.push(song("C", "Artist C", i));
        }

        let mix = DjMixer::sequence_mix(candidates, &db, 15);
        assert_eq!(mix.len(), 15);

        // Verify no consecutive identical artists
        for i in 1..mix.len() {
            assert_ne!(
                mix[i].artist,
                mix[i - 1].artist,
                "Consecutive tracks must not be by the same artist (track {} and {})",
                i - 1,
                i
            );
        }
    }
}
