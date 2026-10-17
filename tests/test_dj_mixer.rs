//! DJ sequencing cooldown rules (issue #21, AGENTS.md §4.3).
//!
//! After a real artist plays, the next `ARTIST_COOLDOWN` (8) tracks must be by other artists;
//! after an album plays, the next `ALBUM_COOLDOWN` (12) tracks must be from other albums.
//! Distances are measured by queue position (untagged / generic tracks count as tracks), the
//! rules are hard whenever an eligible candidate exists, and generic artists are exempt.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;

use quickstream::analysis::mixer::{DjMixer, ALBUM_COOLDOWN, ARTIST_COOLDOWN};
use quickstream::analysis::AnalysisDatabase;
use quickstream::metadata::AudioMetadata;

fn track(artist: &str, album: &str, n: usize) -> AudioMetadata {
    AudioMetadata {
        filepath: PathBuf::from(format!("/music/{}/{}/{}.flac", artist, album, n)),
        title: format!("{} {} {}", artist, album, n),
        artist: artist.to_string(),
        album: album.to_string(),
        ..Default::default()
    }
}

/// Smallest distance (in queue positions) between two plays of the same key.
fn min_gap(seq: &[AudioMetadata], key: impl Fn(&AudioMetadata) -> Option<String>) -> Option<usize> {
    let mut last: HashMap<String, usize> = HashMap::new();
    let mut min: Option<usize> = None;
    for (pos, t) in seq.iter().enumerate() {
        if let Some(k) = key(t) {
            if let Some(prev) = last.insert(k, pos) {
                let gap = pos - prev;
                min = Some(min.map_or(gap, |m: usize| m.min(gap)));
            }
        }
    }
    min
}

fn real_artist(t: &AudioMetadata) -> Option<String> {
    let a = t.artist.trim().to_lowercase();
    (!a.is_empty() && a != "various artists").then_some(a)
}

fn album(t: &AudioMetadata) -> Option<String> {
    let a = t.album.trim().to_lowercase();
    (!a.is_empty()).then_some(a)
}

#[test]
fn test_cooldown_constants_match_agents_md() {
    common::isolate();
    assert_eq!(ARTIST_COOLDOWN, 8);
    assert_eq!(ALBUM_COOLDOWN, 12);
}

/// Nine artists is the minimum that can satisfy an 8-track artist cooldown indefinitely: every
/// repeat must have 8 other tracks in between (gap >= 9), never only 7.
#[test]
fn test_artist_cooldown_with_nine_artists() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let artists = ["A", "B", "C", "D", "E", "F", "G", "H", "I"];
    let mut candidates = Vec::new();
    for a in artists {
        for n in 0..5 {
            // One album per track so only the artist rule is exercised.
            candidates.push(track(a, &format!("{a} album {n}"), n));
        }
    }

    let mix = DjMixer::sequence_mix(candidates, &db, 36);
    assert_eq!(mix.len(), 36);
    let gap = min_gap(&mix, real_artist).expect("artists repeat in a 36-track mix");
    assert!(gap > ARTIST_COOLDOWN, "artist repeated after only {gap} positions");
}

/// Same rule with more artists and uneven track counts per artist.
#[test]
fn test_artist_cooldown_with_uneven_artist_pools() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut candidates = Vec::new();
    for (i, a) in ["Big", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L"]
        .iter()
        .enumerate()
    {
        let count = if i == 0 { 12 } else { 3 };
        for n in 0..count {
            candidates.push(track(a, &format!("{a} album {n}"), n));
        }
    }
    let mix = DjMixer::sequence_mix(candidates, &db, 30);
    assert_eq!(mix.len(), 30);
    let gap = min_gap(&mix, real_artist).unwrap();
    assert!(gap > ARTIST_COOLDOWN, "artist repeated after only {gap} positions");
}

/// Albums shared by different artists (e.g. compilations) need 12 other tracks in between.
#[test]
fn test_album_cooldown_counts_queue_positions() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut candidates = vec![track("Comp One", "Hits", 1), track("Comp Two", "Hits", 2)];
    for n in 0..14 {
        candidates.push(track(&format!("Solo {n}"), &format!("Solo album {n}"), n));
    }
    let mix = DjMixer::sequence_mix(candidates, &db, 16);
    assert_eq!(mix.len(), 16);
    let gap = min_gap(&mix, album).expect("the shared album appears twice");
    assert!(gap > ALBUM_COOLDOWN, "album repeated after only {gap} positions");
}

/// Untagged tracks occupy queue positions too: the cooldown is measured by position in the
/// mix, not by counting only tagged tracks. Untagged / generic tracks are themselves exempt.
#[test]
fn test_untagged_tracks_count_as_positions_and_are_exempt() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut candidates = vec![track("Solo", "Solo A", 1), track("Solo", "Solo B", 2)];
    for n in 0..9 {
        candidates.push(track("", "", n));
    }
    let mix = DjMixer::sequence_mix(candidates, &db, 11);
    assert_eq!(mix.len(), 11);
    let solo: Vec<usize> = mix
        .iter()
        .enumerate()
        .filter(|(_, t)| t.artist == "Solo")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(solo.len(), 2);
    assert!(
        solo[1] - solo[0] > ARTIST_COOLDOWN,
        "Solo repeated at positions {solo:?}"
    );

    // Untagged tracks are allowed back-to-back.
    let untagged_adjacent = mix.windows(2).any(|w| w[0].artist.is_empty() && w[1].artist.is_empty());
    assert!(untagged_adjacent);
}

#[test]
fn test_generic_artists_are_exempt_from_artist_cooldown() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut candidates = Vec::new();
    for n in 0..4 {
        candidates.push(track("Various Artists", &format!("Comp {n}"), n));
    }
    candidates.push(track("Real", "Real album", 0));
    let mix = DjMixer::sequence_mix(candidates, &db, 5);
    assert_eq!(mix.len(), 5, "generic-artist tracks must not be dropped");
}

/// The track already sent to the queue occupies position 0 for cooldown purposes.
#[test]
fn test_already_queued_track_starts_the_artist_cooldown() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let queued = track("Alpha", "Alpha one", 0);
    let mut candidates = vec![track("Alpha", "Alpha two", 1)];
    for n in 0..10 {
        candidates.push(track(&format!("Other {n}"), &format!("Other album {n}"), n));
    }
    let rest = DjMixer::sequence_mix_after(Some(&queued), candidates, &db, 11);
    let alpha = rest
        .iter()
        .position(|t| t.artist == "Alpha")
        .expect("Alpha is still sequenced");
    // Position in the full queue is alpha + 1 (queued track is position 0).
    assert!(
        alpha + 1 > ARTIST_COOLDOWN,
        "Alpha repeated at queue position {}",
        alpha + 1
    );
}

/// When too few artists exist to honour the cooldown, every candidate is still used and the
/// mixer spreads repeats as far apart as possible (never back-to-back when avoidable).
#[test]
fn test_cooldown_falls_back_gracefully_with_few_artists() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut candidates = Vec::new();
    for n in 0..5 {
        for a in ["A", "B", "C"] {
            candidates.push(track(a, &format!("{a} album"), n));
        }
    }
    let mix = DjMixer::sequence_mix(candidates, &db, 15);
    assert_eq!(mix.len(), 15);
    let gap = min_gap(&mix, real_artist).unwrap();
    assert_eq!(gap, 3, "with 3 artists the best achievable spacing is every 3rd track");
}

/// The hard artist rule outranks the album rule and BPM/key flow.
#[test]
fn test_artist_cooldown_beats_bpm_continuity() {
    common::isolate();
    use quickstream::analysis::TrackFeatures;
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let with_bpm = |t: &AudioMetadata, bpm: f64| {
        db.upsert_track(&TrackFeatures {
            filepath: t.filepath.clone(),
            artist: t.artist.clone(),
            bpm: Some(bpm),
            camelot_key: Some("8A".to_string()),
            ..Default::default()
        })
        .unwrap();
    };
    let queued = track("Alpha", "Alpha one", 0);
    with_bpm(&queued, 128.0);
    let same_artist_perfect_match = track("Alpha", "Alpha two", 1);
    with_bpm(&same_artist_perfect_match, 128.0);
    let other_far = track("Beta", "Beta one", 1);
    with_bpm(&other_far, 75.0);

    let rest = DjMixer::sequence_mix_after(Some(&queued), vec![same_artist_perfect_match, other_far], &db, 2);
    assert_eq!(rest[0].artist, "Beta");
}
