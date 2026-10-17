//! Musical key parsing and Camelot Harmonic Mixing wheel representation.

use std::fmt;

/// Camelot key representation (e.g., 8A = A minor, 8B = C major).
/// Numbers 1-12 represent pitch class, Letter 'A' is Minor, 'B' is Major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CamelotKey {
    pub number: u8,     // 1 to 12
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
                1 => 90, // Adjacent key
                2 => 60, // Energy shift
                _ => 20,
            }
        } else {
            // Mixed mode (major <-> minor)
            match num_diff {
                0 => 85, // Relative major/minor (same number)
                1 => 75, // Diagonal harmonic
                _ => 20,
            }
        }
    }
}

impl fmt::Display for CamelotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.number, if self.is_minor { "A" } else { "B" })
    }
}

/// Standard musical key notation parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicalKey;

impl MusicalKey {
    /// Parse raw text tag (from ID3 TKEY, Rekordbox, Traktor, Mixed in Key, or FLAC)
    /// and convert to Camelot notation.
    ///
    /// Accepted forms (surrounding whitespace ignored):
    /// - Camelot: `8A`, `11b`, `08A` (A = minor, B = major).
    /// - Open Key: `1m`..`12m` (minor), `1d`..`12d` (major); `1m` = A minor = `8A`.
    /// - Note + optional accidental + optional mode: `C`, `F#`, `Ab`, `B♭`, `A flat`, `F sharp`,
    ///   followed (optionally after a space, `-` or `_`) by a mode. A single-letter mode is
    ///   case-sensitive (`m` = minor, `M` = major, so `AbM` is A-flat major); longer modes are
    ///   case-insensitive (`min`, `minor`, `maj`, `major`). No mode means major.
    ///
    /// Anything else (unknown modes, out-of-range numbers, trailing text) yields `None`.
    pub fn parse_to_camelot(raw: &str) -> Option<CamelotKey> {
        let clean = raw.trim();
        if clean.is_empty() {
            return None;
        }
        Self::parse_numbered(clean).or_else(|| Self::parse_note_and_mode(clean))
    }

    /// Camelot (`8A`) or Open Key (`1m` / `1d`) notation: a number followed by one letter.
    fn parse_numbered(clean: &str) -> Option<CamelotKey> {
        let split = clean.find(|c: char| !c.is_ascii_digit())?;
        let (digits, rest) = clean.split_at(split);
        if digits.is_empty() || rest.chars().count() != 1 {
            return None;
        }
        let num: u8 = digits.parse().ok()?;
        if !(1..=12).contains(&num) {
            return None;
        }
        match rest.to_ascii_lowercase().as_str() {
            "a" => CamelotKey::new(num, true),
            "b" => CamelotKey::new(num, false),
            // Open Key 1 is C major / A minor, which Camelot numbers 8.
            "m" => CamelotKey::new((num + 6) % 12 + 1, true),
            "d" => CamelotKey::new((num + 6) % 12 + 1, false),
            _ => None,
        }
    }

    /// Note letter, optional accidental, optional mode suffix.
    fn parse_note_and_mode(clean: &str) -> Option<CamelotKey> {
        let mut chars = clean.chars();
        let letter = chars.next()?.to_ascii_uppercase();
        let natural: i32 = match letter {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            'B' => 11,
            _ => return None,
        };
        let rest = chars.as_str();

        let (accidental, rest) = Self::split_accidental(rest);
        let pitch_class = (natural + accidental).rem_euclid(12);

        let mode = rest.trim_start_matches([' ', '-', '_']);
        let is_minor = match mode {
            "" | "M" => false,
            "m" => true,
            _ => match mode.to_ascii_lowercase().as_str() {
                "maj" | "major" => false,
                "min" | "minor" => true,
                _ => return None,
            },
        };

        // Camelot numbers walk the circle of fifths: C major = 8B, G major = 9B, ...
        // A minor key shares its number with its relative major (3 semitones up).
        let major_pc = if is_minor { (pitch_class + 3) % 12 } else { pitch_class };
        let number = ((major_pc * 7) % 12 + 7) % 12 + 1;
        CamelotKey::new(number as u8, is_minor)
    }

    /// Split a leading accidental (`#`, `b`, `♯`, `♭`, `sharp`, `flat`) off `rest`, returning its
    /// semitone offset and the remainder.
    fn split_accidental(rest: &str) -> (i32, &str) {
        for (token, offset) in [("#", 1), ("♯", 1), ("b", -1), ("♭", -1)] {
            if let Some(after) = rest.strip_prefix(token) {
                return (offset, after);
            }
        }
        let spaced = rest.trim_start_matches([' ', '-', '_']);
        let lower = spaced.to_ascii_lowercase();
        for (word, offset) in [("sharp", 1), ("flat", -1)] {
            if lower.starts_with(word) {
                return (offset, &spaced[word.len()..]);
            }
        }
        (0, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_camelot_key_parsing() {
        assert_eq!(
            MusicalKey::parse_to_camelot("8A"),
            Some(CamelotKey {
                number: 8,
                is_minor: true
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("11B"),
            Some(CamelotKey {
                number: 11,
                is_minor: false
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("Am"),
            Some(CamelotKey {
                number: 8,
                is_minor: true
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("A minor"),
            Some(CamelotKey {
                number: 8,
                is_minor: true
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("C"),
            Some(CamelotKey {
                number: 8,
                is_minor: false
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("C Maj"),
            Some(CamelotKey {
                number: 8,
                is_minor: false
            })
        );
        assert_eq!(
            MusicalKey::parse_to_camelot("C#m"),
            Some(CamelotKey {
                number: 12,
                is_minor: true
            })
        );
    }

    fn minor(n: u8) -> Option<CamelotKey> {
        Some(CamelotKey {
            number: n,
            is_minor: true,
        })
    }
    fn major(n: u8) -> Option<CamelotKey> {
        Some(CamelotKey {
            number: n,
            is_minor: false,
        })
    }

    #[test]
    fn test_key_parsing_table() {
        let cases: &[(&str, Option<CamelotKey>)] = &[
            // Camelot (any case, optional zero padding / whitespace)
            ("8A", minor(8)),
            ("11B", major(11)),
            ("1a", minor(1)),
            ("12b", major(12)),
            ("08A", minor(8)),
            (" 5B ", major(5)),
            // Open Key: 1m = A minor (8A), 1d = C major (8B), 6d = B major (1B), 12m = D minor (7A)
            ("1m", minor(8)),
            ("1d", major(8)),
            ("6d", major(1)),
            ("12m", minor(7)),
            ("5M", minor(12)),
            ("10D", major(5)),
            // Single-letter mode suffix: lower-case m = minor, upper-case M = major
            ("Am", minor(8)),
            ("AbM", major(4)),
            ("Abm", minor(1)),
            ("C#m", minor(12)),
            ("Gm", minor(6)),
            ("GM", major(9)),
            ("F#", major(2)),
            ("C", major(8)),
            ("E", major(12)),
            // Spelled-out / abbreviated modes, any case, optional separator
            ("Ab major", major(4)),
            ("F# Minor", minor(11)),
            ("Emaj", major(12)),
            ("G#min", minor(1)),
            ("A minor", minor(8)),
            ("C Maj", major(8)),
            ("D MINOR", minor(7)),
            ("Bb-major", major(6)),
            ("Eb_min", minor(2)),
            ("db minor", minor(12)),
            // Unicode and spelled-out accidentals, enharmonics
            ("B♭m", minor(3)),
            ("F♯ major", major(2)),
            ("A flat minor", minor(1)),
            ("F sharp minor", minor(11)),
            ("Cb", major(1)),
            ("E#m", minor(4)),
            // Garbage
            ("", None),
            ("   ", None),
            ("13A", None),
            ("0B", None),
            ("8C", None),
            ("0m", None),
            ("H", None),
            ("Am major", None),
            ("A mixolydian", None),
            ("o", None),
        ];
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|(raw, expected)| {
                let got = MusicalKey::parse_to_camelot(raw);
                (got != *expected).then(|| format!("{raw:?}: expected {expected:?}, got {got:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "key parsing mismatches:\n{}", failures.join("\n"));
    }

    /// Every pitch class in both modes maps to a distinct Camelot slot.
    #[test]
    fn test_all_notes_map_to_distinct_camelot_keys() {
        let notes = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];
        let mut seen = std::collections::HashSet::new();
        for note in notes {
            for suffix in ["", "m"] {
                let key = MusicalKey::parse_to_camelot(&format!("{note}{suffix}"))
                    .unwrap_or_else(|| panic!("{note}{suffix} must parse"));
                assert!(seen.insert(key), "{note}{suffix} collides at {key}");
            }
        }
        assert_eq!(seen.len(), 24);
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
