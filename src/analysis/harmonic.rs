//! Musical key parsing and Camelot Harmonic Mixing wheel representation.

use std::fmt;

/// Camelot key representation (e.g., 8A = A minor, 8B = C major).
/// Numbers 1-12 represent pitch class, Letter 'A' is Minor, 'B' is Major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CamelotKey {
    pub number: u8, // 1 to 12
    pub is_minor: bool, // true = A (minor), false = B (major)
}

impl CamelotKey {
    pub fn new(number: u8, is_minor: bool) -> Option<Self> {
        if (1..=12).contains(&number) {
            Some(Self { number, is_minor })
        } else {
            None
        }
    }

    /// Calculate harmonic mixing compatibility score (0 - 100).
    /// DJ Camelot rules:
    /// - 100: Exact same key (8A -> 8A)
    /// - 90: Adjacent key on wheel (8A -> 7A or 9A)
    /// - 85: Relative major/minor (8A -> 8B)
    /// - 75: Diagonal transition (8A -> 7B or 9B)
    /// - 60: Energy boost (+2 or -2 on wheel, e.g. 8A -> 10A)
    /// - 20: Dissonant transition
    pub fn compatibility_score(&self, other: &Self) -> u32 {
        if self == other {
            return 100;
        }

        let num_diff = if self.number > other.number {
            (self.number - other.number).min(12 - (self.number - other.number))
        } else {
            (other.number - self.number).min(12 - (other.number - self.number))
        };

        if self.is_minor == other.is_minor {
            match num_diff {
                1 => 90,  // Adjacent key
                2 => 60,  // Energy shift
                _ => 20,
            }
        } else {
            // Mixed mode (major <-> minor)
            match num_diff {
                0 => 85,  // Relative major/minor (same number)
                1 => 75,  // Diagonal harmonic
                _ => 20,
            }
        }
    }
}

impl fmt::Display for CamelotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            self.number,
            if self.is_minor { "A" } else { "B" }
        )
    }
}

/// Standard musical key notation parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicalKey;

impl MusicalKey {
    /// Parse raw text tag (from ID3 TKEY, Rekordbox, Traktor, Mixed in Key, or FLAC)
    /// and convert to Camelot notation.
    pub fn parse_to_camelot(raw: &str) -> Option<CamelotKey> {
        let clean = raw.trim().to_uppercase();
        if clean.is_empty() {
            return None;
        }

        // 1. Direct Camelot notation: e.g. "8A", "11B", "1A"
        if let Some(caps) = clean.strip_suffix('A') {
            if let Ok(num) = caps.parse::<u8>() {
                return CamelotKey::new(num, true);
            }
        } else if let Some(caps) = clean.strip_suffix('B') {
            if let Ok(num) = caps.parse::<u8>() {
                return CamelotKey::new(num, false);
            }
        }

        // 2. Standard Musical Key Notation (e.g. "Am", "A minor", "C#m", "Db Maj", "AbM")
        // Camelot Mapping:
        // Minor:
        // Abm/G#m: 1A, Ebm/D#m: 2A, Bbm/A#m: 3A, Fm: 4A, Cm: 5A, Gm: 6A, Dm: 7A, Am: 8A, Em: 9A, Bm: 10A, F#m/Gbm: 11A, C#m/Dbm: 12A
        // Major:
        // B: 1B, F#/Gb: 2B, Db/C#: 3B, Ab/G#: 4B, Eb/D#: 5B, Bb/A#: 6B, F: 7B, C: 8B, G: 9B, D: 10B, A: 11B, E: 12B
        let is_minor = clean.contains("MIN") || clean.contains("M") && !clean.contains("MAJ") || clean.ends_with('M');
        let note = clean
            .replace("MINOR", "")
            .replace("MAJOR", "")
            .replace("MIN", "")
            .replace("MAJ", "")
            .replace('M', "")
            .trim()
            .to_string();

        let num = match (note.as_str(), is_minor) {
            // Minor keys
            ("AB" | "G#", true) => 1,
            ("EB" | "D#", true) => 2,
            ("BB" | "A#", true) => 3,
            ("F", true) => 4,
            ("C", true) => 5,
            ("G", true) => 6,
            ("D", true) => 7,
            ("A", true) => 8,
            ("E", true) => 9,
            ("B", true) => 10,
            ("F#" | "GB", true) => 11,
            ("C#" | "DB", true) => 12,

            // Major keys
            ("B", false) => 1,
            ("F#" | "GB", false) => 2,
            ("DB" | "C#", false) => 3,
            ("AB" | "G#", false) => 4,
            ("EB" | "D#", false) => 5,
            ("BB" | "A#", false) => 6,
            ("F", false) => 7,
            ("C", false) => 8,
            ("G", false) => 9,
            ("D", false) => 10,
            ("A", false) => 11,
            ("E", false) => 12,

            _ => return None,
        };

        CamelotKey::new(num, is_minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_camelot_key_parsing() {
        assert_eq!(MusicalKey::parse_to_camelot("8A"), Some(CamelotKey { number: 8, is_minor: true }));
        assert_eq!(MusicalKey::parse_to_camelot("11B"), Some(CamelotKey { number: 11, is_minor: false }));
        assert_eq!(MusicalKey::parse_to_camelot("Am"), Some(CamelotKey { number: 8, is_minor: true }));
        assert_eq!(MusicalKey::parse_to_camelot("A minor"), Some(CamelotKey { number: 8, is_minor: true }));
        assert_eq!(MusicalKey::parse_to_camelot("C"), Some(CamelotKey { number: 8, is_minor: false }));
        assert_eq!(MusicalKey::parse_to_camelot("C Maj"), Some(CamelotKey { number: 8, is_minor: false }));
        assert_eq!(MusicalKey::parse_to_camelot("C#m"), Some(CamelotKey { number: 12, is_minor: true }));
    }

    #[test]
    fn test_camelot_compatibility_scoring() {
        let key_8a = CamelotKey::new(8, true).unwrap();
        let key_8b = CamelotKey::new(8, false).unwrap();
        let key_9a = CamelotKey::new(9, true).unwrap();
        let key_7a = CamelotKey::new(7, true).unwrap();
        let key_2a = CamelotKey::new(2, true).unwrap();

        // Exact match
        assert_eq!(key_8a.compatibility_score(&key_8a), 100);
        // Adjacent
        assert_eq!(key_8a.compatibility_score(&key_9a), 90);
        assert_eq!(key_8a.compatibility_score(&key_7a), 90);
        // Relative major/minor
        assert_eq!(key_8a.compatibility_score(&key_8b), 85);
        // Distant
        assert_eq!(key_8a.compatibility_score(&key_2a), 20);
    }
}
