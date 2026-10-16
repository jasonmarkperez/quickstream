//! Intelligent Mix Engine: sequences tracks into cohesive mixes using tempo, harmonic keys, and mood.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use crate::metadata::AudioMetadata;
use super::db::{AnalysisDatabase, TrackFeatures};
use super::harmonic::MusicalKey;

/// Mood presets for intelligent vibe mixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoodType {
    Chill,     // Ambient, downtempo, acoustic, classical, lofi (60-95 BPM, low energy)
    Workout,   // Electronic, rock, dance, hip-hop (125-140 BPM, high energy)
    Focus,     // Instrumental, synthwave, minimalist, piano
    Groove,    // Funk, soul, disco, house (115-128 BPM)
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
        let (min_bpm, max_bpm) = self.bpm_range();
        let bpm_ok = feat.bpm.map(|b| b >= min_bpm && b <= max_bpm).unwrap_or(true);

        let tags_lower = feat.tags_json.to_lowercase();
        let genre_lower = feat.genre.to_lowercase();
        let combined = format!("{} {}", tags_lower, genre_lower);

        match self {
            MoodType::Chill => {
                bpm_ok && (
                    combined.contains("chill")
                    || combined.contains("ambient")
                    || combined.contains("acoustic")
                    || combined.contains("classical")
                    || combined.contains("downtempo")
                    || combined.contains("lofi")
                    || feat.energy.map(|e| e <= 0.50).unwrap_or(false)
                )
            }
            MoodType::Workout => {
                bpm_ok && (
                    combined.contains("dance")
                    || combined.contains("electronic")
                    || combined.contains("rock")
                    || combined.contains("house")
                    || combined.contains("techno")
                    || combined.contains("upbeat")
                    || feat.energy.map(|e| e >= 0.65).unwrap_or(false)
                )
            }
            MoodType::Focus => {
                combined.contains("instrumental")
                || combined.contains("piano")
                || combined.contains("soundtrack")
                || combined.contains("synthwave")
                || combined.contains("minimal")
                || combined.contains("ambient")
            }
            MoodType::Groove => {
                bpm_ok && (
                    combined.contains("funk")
                    || combined.contains("soul")
                    || combined.contains("disco")
                    || combined.contains("groove")
                    || combined.contains("r&b")
                    || combined.contains("house")
                )
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
        if candidates.is_empty() {
            return Vec::new();
        }

        // 1. Gather TrackFeatures for each candidate (or synthesize defaults)
        let mut track_items: Vec<(AudioMetadata, Option<TrackFeatures>)> = candidates
            .into_iter()
            .map(|meta| {
                let feat = features_db.get_track(&meta.filepath);
                (meta, feat)
            })
            .collect();

        // 2. Greedy sequencing with DJ rules:
        // - Avoid same artist within 8 tracks
        // - Avoid same album within 12 tracks
        // - Minimize |BPM_{i} - BPM_{i-1}|
        let mut sequenced: Vec<AudioMetadata> = Vec::new();
        let mut recent_artists: Vec<String> = Vec::new();
        let mut recent_albums: Vec<String> = Vec::new();

        while !track_items.is_empty() && sequenced.len() < target_limit {
            let last_bpm = sequenced
                .last()
                .and_then(|m| features_db.get_track(&m.filepath))
                .and_then(|f| f.bpm);

            let last_camelot = sequenced
                .last()
                .and_then(|m| features_db.get_track(&m.filepath))
                .and_then(|f| f.camelot_key.as_deref().and_then(MusicalKey::parse_to_camelot));

            let mut best_idx = 0;
            let mut best_score = i64::MIN;

            for (idx, (meta, feat)) in track_items.iter().enumerate() {
                let mut score: i64 = 100;

                // Penalize repeating recent artists
                if !meta.artist.is_empty() {
                    if let Some(pos) = recent_artists.iter().rposition(|a| a == &meta.artist) {
                        let distance = recent_artists.len() - pos;
                        if distance < 8 {
                            score -= (8 - distance as i64) * 40;
                        }
                    }
                }

                // Penalize repeating recent albums
                if !meta.album.is_empty() {
                    if let Some(pos) = recent_albums.iter().rposition(|a| a == &meta.album) {
                        let distance = recent_albums.len() - pos;
                        if distance < 12 {
                            score -= (12 - distance as i64) * 25;
                        }
                    }
                }

                // Score BPM continuity
                if let (Some(cur_bpm), Some(prev_bpm)) = (feat.as_ref().and_then(|f| f.bpm), last_bpm) {
                    let diff = (cur_bpm - prev_bpm).abs();
                    if diff <= 4.0 {
                        score += 50; // Smooth blend
                    } else if diff <= 8.0 {
                        score += 20;
                    } else {
                        score -= (diff as i64) * 3;
                    }
                }

                // Score harmonic key compatibility
                if let (Some(cur_cam), Some(prev_cam)) = (
                    feat.as_ref().and_then(|f| f.camelot_key.as_deref().and_then(MusicalKey::parse_to_camelot)),
                    last_camelot,
                ) {
                    let harm_score = prev_cam.compatibility_score(&cur_cam);
                    score += harm_score as i64 / 2;
                }

                if score > best_score {
                    best_score = score;
                    best_idx = idx;
                }
            }

            let (chosen_meta, _) = track_items.remove(best_idx);
            if !chosen_meta.artist.is_empty() {
                recent_artists.push(chosen_meta.artist.clone());
            }
            if !chosen_meta.album.is_empty() {
                recent_albums.push(chosen_meta.album.clone());
            }
            sequenced.push(chosen_meta);
        }

        sequenced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dj_mixer_spacing_rules() {
        let db = AnalysisDatabase::open_in_memory().unwrap();
        let mut candidates = Vec::new();

        // 5 songs by Artist A, 5 songs by Artist B, 5 songs by Artist C
        for i in 1..=5 {
            let mut m1 = AudioMetadata::default();
            m1.filepath = PathBuf::from(format!("/music/A/{}.mp3", i));
            m1.artist = "Artist A".to_string();
            m1.title = format!("Song A{}", i);
            candidates.push(m1);

            let mut m2 = AudioMetadata::default();
            m2.filepath = PathBuf::from(format!("/music/B/{}.mp3", i));
            m2.artist = "Artist B".to_string();
            m2.title = format!("Song B{}", i);
            candidates.push(m2);

            let mut m3 = AudioMetadata::default();
            m3.filepath = PathBuf::from(format!("/music/C/{}.mp3", i));
            m3.artist = "Artist C".to_string();
            m3.title = format!("Song C{}", i);
            candidates.push(m3);
        }

        let mix = DjMixer::sequence_mix(candidates, &db, 15);
        assert_eq!(mix.len(), 15);

        // Verify no consecutive identical artists
        for i in 1..mix.len() {
            assert_ne!(
                mix[i].artist, mix[i - 1].artist,
                "Consecutive tracks must not be by the same artist (track {} and {})",
                i - 1, i
            );
        }
    }
}
