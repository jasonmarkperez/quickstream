//! Playlists (issue #37): the M3U8 format, playlist names and queue planning.

mod common;

use std::path::{Path, PathBuf};

use quickstream::metadata::AudioMetadata;
use quickstream::playlist::{
    names_equal, normalize_path, parse_m3u8, placeholder_metadata, plan_queue, playlist_file_path, validate_name,
    NameError, Playlist, PlaylistEntry, MAX_NAME_CHARS,
};

fn entry(path: &str, secs: i64, artist: &str, title: &str) -> PlaylistEntry {
    PlaylistEntry {
        path: PathBuf::from(path),
        duration_secs: secs,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

fn road_trip() -> Playlist {
    Playlist {
        name: "Road Trip".to_string(),
        entries: vec![
            entry(
                "/Volumes/Music/Radiohead/OK Computer/01 Airbag.flac",
                284,
                "Radiohead",
                "Airbag",
            ),
            entry("/Volumes/Music/Untagged/track.mp3", -1, "", "track"),
        ],
    }
}

#[test]
fn test_playlists_dir_is_under_config_dir() {
    let root = common::isolate();
    let dir = quickstream::paths::get_playlists_dir();
    assert_eq!(dir, quickstream::paths::get_config_dir().join("playlists"));
    assert!(dir.starts_with(root), "{} is outside {}", dir.display(), root.display());
}

#[test]
fn test_to_m3u8_writes_extended_m3u() {
    common::isolate();
    let text = road_trip().to_m3u8();
    assert_eq!(
        text,
        "#EXTM3U\n\
         #PLAYLIST:Road Trip\n\
         #EXTINF:284,Radiohead - Airbag\n\
         /Volumes/Music/Radiohead/OK Computer/01 Airbag.flac\n\
         #EXTINF:-1,track\n\
         /Volumes/Music/Untagged/track.mp3\n"
    );
    assert!(!text.starts_with('\u{feff}'));
    assert!(!text.contains('\r'));
}

#[test]
fn test_m3u8_round_trip() {
    common::isolate();
    let original = road_trip();
    let parsed = parse_m3u8("Road Trip", &original.to_m3u8(), Path::new("/unused"));
    assert_eq!(parsed, original);
}

#[test]
fn test_m3u8_round_trip_non_ascii() {
    common::isolate();
    let original = Playlist {
        name: "Café del Mar ☕".to_string(),
        entries: vec![
            entry("/Volumes/Música/Björk/Homogenic/03 Jóga.flac", 305, "Björk", "Jóga"),
            entry(
                "/Volumes/Music/坂本龍一/戦場のメリークリスマス.mp3",
                271,
                "坂本龍一",
                "戦場のメリークリスマス",
            ),
        ],
    };
    let text = original.to_m3u8();
    assert!(text.contains("#PLAYLIST:Café del Mar ☕\n"));
    assert_eq!(parse_m3u8("Café del Mar ☕", &text, Path::new("/unused")), original);
}

#[test]
fn test_to_m3u8_keeps_control_characters_out_of_info_lines() {
    common::isolate();
    let mut playlist = Playlist::new("Tags");
    playlist.entries.push(entry("/m/a.flac", 10, "Art\nist", "Ti\r\ntle"));
    let text = playlist.to_m3u8();
    assert_eq!(text.lines().count(), 4, "{:?}", text);
    let parsed = parse_m3u8("Tags", &text, Path::new("/unused"));
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].path, PathBuf::from("/m/a.flac"));
}

#[test]
fn test_parse_ignores_playlist_line_and_uses_given_name() {
    common::isolate();
    let parsed = parse_m3u8(
        "road trip",
        "#EXTM3U\n#PLAYLIST:Something Else\n/m/a.mp3\n",
        Path::new("/pl"),
    );
    assert_eq!(parsed.name, "road trip");
    assert_eq!(parsed.entries.len(), 1);
}

#[test]
fn test_parse_strips_bom() {
    common::isolate();
    let parsed = parse_m3u8("p", "\u{feff}#EXTM3U\n#EXTINF:10,A - B\n/m/a.mp3\n", Path::new("/pl"));
    assert_eq!(parsed.entries, vec![entry("/m/a.mp3", 10, "A", "B")]);

    // A BOM right before a path line (no header) must not end up in the path.
    let parsed = parse_m3u8("p", "\u{feff}/m/a.mp3\n", Path::new("/pl"));
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].path, PathBuf::from("/m/a.mp3"));
}

#[test]
fn test_parse_accepts_crlf() {
    common::isolate();
    let parsed = parse_m3u8(
        "p",
        "#EXTM3U\r\n#EXTINF:10,A - B\r\n/m/a.mp3\r\n/m/b.mp3\r\n",
        Path::new("/pl"),
    );
    assert_eq!(
        parsed.entries,
        vec![entry("/m/a.mp3", 10, "A", "B"), entry("/m/b.mp3", -1, "", "b")]
    );
}

#[test]
fn test_parse_skips_unknown_hash_lines_and_blank_lines() {
    common::isolate();
    let text = "#EXTM3U\n\
                # a comment\n\
                \n\
                #EXTINF:200,Artist - Title\n\
                #EXTALB:Some Album\n\
                \n\
                #EXTGRP:Group\n\
                /m/a.flac\n\
                \n\
                \n\
                /m/b.flac\n";
    let parsed = parse_m3u8("p", text, Path::new("/pl"));
    assert_eq!(
        parsed.entries,
        vec![
            entry("/m/a.flac", 200, "Artist", "Title"),
            entry("/m/b.flac", -1, "", "b"),
        ]
    );
}

#[test]
fn test_parse_resolves_relative_paths_against_base_dir() {
    common::isolate();
    let base = Path::new("/Users/me/.config/quickstream/playlists");
    let parsed = parse_m3u8("p", "Sub Dir/a.mp3\n../Music/b.flac\n/abs/c.ogg\n", base);
    let paths: Vec<PathBuf> = parsed.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            base.join("Sub Dir/a.mp3"),
            PathBuf::from("/Users/me/.config/quickstream/Music/b.flac"),
            PathBuf::from("/abs/c.ogg"),
        ]
    );
}

/// Issue #58: a hand-written `..` entry is resolved when read, so the first edit never writes
/// `<playlists>/../../..` back and the path does not depend on what the playlists folder is.
#[test]
fn test_parse_normalizes_dot_dot_and_dot_entries() {
    common::isolate();
    let base = Path::new("/home/me/.config/quickstream/playlists");
    let text = "../../../../music/x.mp3\n./here.mp3\n/m/a/../b/./c.flac\n/../../root.mp3\n";
    let parsed = parse_m3u8("p", text, base);
    let paths: Vec<PathBuf> = parsed.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            PathBuf::from("/home/music/x.mp3"),
            base.join("here.mp3"),
            PathBuf::from("/m/b/c.flac"),
            PathBuf::from("/root.mp3"),
        ]
    );
    // The file stem fallback title still comes from the file name.
    assert_eq!(parsed.entries[0].title, "x");
}

#[test]
fn test_write_after_parse_contains_no_dot_dot() {
    common::isolate();
    let base = Path::new("/home/me/.config/quickstream/playlists");
    let parsed = parse_m3u8("p", "#EXTINF:10,A - X\n../../../../music/x.mp3\n/m/a/../y.mp3\n", base);
    let written = parsed.to_m3u8();
    assert!(!written.contains(".."), "written playlist keeps `..`: {:?}", written);
    assert!(written.contains("\n/home/music/x.mp3\n"), "{:?}", written);
    assert!(written.contains("\n/m/y.mp3\n"), "{:?}", written);
    assert_eq!(parse_m3u8("p", &written, base), parsed);
}

#[test]
fn test_normalize_path_is_lexical() {
    common::isolate();
    let cases = [
        ("/a/./b/../c", "/a/c"),
        ("/../x", "/x"),
        ("/../../x/../../y", "/y"),
        ("/a/b/..", "/a"),
        ("/a/b/.", "/a/b"),
        ("/a/b/", "/a/b"),
        ("/a//b", "/a/b"),
        ("/a/b/../..", "/"),
        ("/", "/"),
        ("/a/b/c.flac", "/a/b/c.flac"),
        ("/Music/..hidden/x...mp3", "/Music/..hidden/x...mp3"),
        // Relative paths keep a leading `..` that has nothing to cancel.
        ("a/../b", "b"),
        ("./a/./b", "a/b"),
        ("../a", "../a"),
        ("a/../../b", "../b"),
        ("a/..", "."),
        (".", "."),
    ];
    for (input, expected) in cases {
        assert_eq!(
            normalize_path(Path::new(input)).as_os_str(),
            Path::new(expected).as_os_str(),
            "normalize_path({:?})",
            input
        );
    }
}

/// Lexical only: the disk is never read, so a symlink is not followed and a path that does not
/// exist normalizes like any other.
#[test]
fn test_normalize_path_does_not_follow_symlinks() {
    let root = common::isolate();
    let dir = root.join("normalize_symlink");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("real/deep")).unwrap();
    std::os::unix::fs::symlink(dir.join("real/deep"), dir.join("link")).unwrap();
    // The OS would resolve `link/..` to `real`; lexically it is `dir` itself.
    assert_eq!(normalize_path(&dir.join("link/../x.mp3")), dir.join("x.mp3"));
    assert_eq!(
        normalize_path(Path::new("/nonexistent/qs/../a.mp3")),
        PathBuf::from("/nonexistent/a.mp3")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_parse_extinf_durations() {
    common::isolate();
    let text = "#EXTINF:-1,A - Unknown length\n/m/a.mp3\n\
                #EXTINF:284.6,B - Float\n/m/b.mp3\n\
                #EXTINF:abc,C - Garbage\n/m/c.mp3\n\
                #EXTINF:-1 tvg-id=\"x\",D - Attributes\n/m/d.mp3\n\
                #extinf:42,E - Lower case tag\n/m/e.mp3\n\
                #EXTINF:1e300,F - Huge\n/m/f.mp3\n\
                #EXTINF:1e300,G - Huge\n/m/g.mp3\n";
    let parsed = parse_m3u8("p", text, Path::new("/pl"));
    let secs: Vec<i64> = parsed.entries.iter().map(|e| e.duration_secs).collect();
    assert_eq!(&secs[..5], &[-1, 285, -1, -1, 42]);
    assert_eq!(parsed.entries[3].title, "Attributes");
    assert_eq!(parsed.entries[4].artist, "E");
    // Absurd durations must not overflow the total.
    assert!(parsed.total_duration_secs() > 0);
}

#[test]
fn test_parse_entry_without_extinf_uses_file_stem() {
    common::isolate();
    let parsed = parse_m3u8("p", "#EXTM3U\n/m/Artist/01 Song.flac\n", Path::new("/pl"));
    assert_eq!(parsed.entries, vec![entry("/m/Artist/01 Song.flac", -1, "", "01 Song")]);

    // `#EXTINF` with no text after the comma also falls back to the file stem.
    let parsed = parse_m3u8("p", "#EXTINF:12,\n/m/Artist/02 Other.flac\n", Path::new("/pl"));
    assert_eq!(
        parsed.entries,
        vec![entry("/m/Artist/02 Other.flac", 12, "", "02 Other")]
    );
}

#[test]
fn test_parse_extinf_without_separator_is_title() {
    common::isolate();
    let parsed = parse_m3u8("p", "#EXTINF:12,Just A Title\n/m/a.mp3\n", Path::new("/pl"));
    assert_eq!(parsed.entries, vec![entry("/m/a.mp3", 12, "", "Just A Title")]);
}

#[test]
fn test_artist_containing_separator_keeps_path() {
    common::isolate();
    let mut playlist = Playlist::new("p");
    playlist.entries.push(entry(
        "/m/Simon - Garfunkel/The Boxer.flac",
        308,
        "Simon - Garfunkel",
        "The Boxer",
    ));
    let parsed = parse_m3u8("p", &playlist.to_m3u8(), Path::new("/pl"));
    assert_eq!(parsed.entries.len(), 1);
    let got = &parsed.entries[0];
    // The path line is the source of truth; the split only affects display.
    assert_eq!(got.path, PathBuf::from("/m/Simon - Garfunkel/The Boxer.flac"));
    assert_eq!(got.duration_secs, 308);
    assert_eq!(got.display_label(), "Simon - Garfunkel - The Boxer");
}

#[test]
fn test_parse_foreign_path_styles_without_panicking() {
    common::isolate();
    let base = Path::new("/pl");
    let windows = r"C:\Music\Artist\01 Song.mp3";
    let url = "file:///Volumes/Music/a.mp3";
    let parsed = parse_m3u8("p", &format!("{}\n{}\n", windows, url), base);
    assert_eq!(parsed.entries.len(), 2);
    // Both are kept as plain path entries; they show up as missing later.
    assert_eq!(parsed.entries[0].path, base.join(windows));
    assert_eq!(parsed.entries[1].path, base.join(url));
    for e in &parsed.entries {
        assert_eq!(e.duration_secs, -1);
        assert_eq!(e.artist, "");
        assert!(!e.title.is_empty());
    }
}

#[test]
fn test_entry_from_metadata_and_from_path() {
    common::isolate();
    let meta = AudioMetadata {
        filepath: PathBuf::from("/m/a.flac"),
        filename: "a.flac".to_string(),
        title: "Airbag".to_string(),
        artist: "Radiohead".to_string(),
        duration_seconds: 283.6,
        ..Default::default()
    };
    assert_eq!(
        PlaylistEntry::from_metadata(&meta),
        entry("/m/a.flac", 284, "Radiohead", "Airbag")
    );

    // No title tag: the display title (file name); no duration: unknown.
    let untagged = AudioMetadata {
        filepath: PathBuf::from("/m/b.mp3"),
        filename: "b.mp3".to_string(),
        ..Default::default()
    };
    assert_eq!(
        PlaylistEntry::from_metadata(&untagged),
        entry("/m/b.mp3", -1, "", "b.mp3")
    );

    // `from_path` reads nothing: a path that does not exist is fine.
    assert_eq!(
        PlaylistEntry::from_path(Path::new("/qs-test-nonexistent/Artist/01 Song.flac")),
        entry("/qs-test-nonexistent/Artist/01 Song.flac", -1, "", "01 Song")
    );
}

#[test]
fn test_display_label_and_playlist_helpers() {
    common::isolate();
    assert_eq!(entry("/m/a.mp3", 1, "A", "T").display_label(), "A - T");
    assert_eq!(entry("/m/a.mp3", 1, "", "T").display_label(), "T");

    let mut playlist = road_trip();
    playlist.entries.push(entry("/m/c.mp3", 16, "C", "C"));
    // The unknown (-1) duration is left out of the total.
    assert_eq!(playlist.total_duration_secs(), 300);
    assert!(playlist.contains_path(Path::new("/Volumes/Music/Untagged/track.mp3")));
    assert!(!playlist.contains_path(Path::new("/Volumes/Music/Untagged/other.mp3")));
    assert_eq!(Playlist::new("Empty").total_duration_secs(), 0);
}

#[test]
fn test_validate_name_rejections() {
    common::isolate();
    assert_eq!(validate_name(""), Err(NameError::Empty));
    assert_eq!(validate_name("   \t "), Err(NameError::Empty));
    assert_eq!(validate_name(&"a".repeat(MAX_NAME_CHARS + 1)), Err(NameError::TooLong));
    assert_eq!(validate_name("AC/DC"), Err(NameError::Slash));
    assert_eq!(validate_name("../escape"), Err(NameError::Slash));
    assert_eq!(validate_name(r"back\slash"), Err(NameError::Backslash));
    assert_eq!(validate_name("nul\0byte"), Err(NameError::ControlChar));
    assert_eq!(validate_name("tab\tinside"), Err(NameError::ControlChar));
    assert_eq!(validate_name("line\nbreak"), Err(NameError::ControlChar));
    assert_eq!(validate_name(".hidden"), Err(NameError::LeadingDot));
    assert_eq!(validate_name("  .hidden"), Err(NameError::LeadingDot));
}

#[test]
fn test_validate_name_trims_and_counts_characters() {
    common::isolate();
    assert_eq!(validate_name("  Road Trip \t"), Ok("Road Trip".to_string()));
    assert_eq!(validate_name("Café del Mar ☕"), Ok("Café del Mar ☕".to_string()));
    assert_eq!(validate_name("Mix v1.2"), Ok("Mix v1.2".to_string()));
    // The limit is in characters, not bytes.
    let max = "é".repeat(MAX_NAME_CHARS);
    assert_eq!(validate_name(&max), Ok(max.clone()));
    assert_eq!(validate_name(&format!("{}é", max)), Err(NameError::TooLong));
}

#[test]
fn test_name_error_messages() {
    common::isolate();
    assert_eq!(NameError::Empty.to_string(), "Name can't be empty");
    assert_eq!(NameError::TooLong.to_string(), "Name is too long (max 100 characters)");
    assert_eq!(NameError::Slash.to_string(), "Name can't contain /");
    assert_eq!(NameError::Backslash.to_string(), "Name can't contain \\");
    assert_eq!(
        NameError::ControlChar.to_string(),
        "Name can't contain control characters"
    );
    assert_eq!(NameError::LeadingDot.to_string(), "Name can't start with .");
    assert_eq!(
        NameError::Duplicate("Road Trip".to_string()).to_string(),
        "\"Road Trip\" already exists"
    );
}

#[test]
fn test_names_equal_ignores_case() {
    common::isolate();
    assert!(names_equal("road trip", "Road Trip"));
    assert!(names_equal("Café", "CAFÉ"));
    assert!(!names_equal("Road Trip", "Road Trip 2"));
}

#[test]
fn test_names_equal_ignores_unicode_normalization() {
    common::isolate();
    // Issue #55: APFS treats NFC `Zoë` and NFD `Zoë` (e + combining diaeresis) as one file name.
    assert!(names_equal("Zo\u{eb}", "Zoe\u{308}"));
    assert!(names_equal("ZO\u{cb}", "zoe\u{308}"));
    assert!(!names_equal("Zo\u{eb}", "Zoe"));
}

#[test]
fn test_names_equal_uses_full_case_folding() {
    common::isolate();
    // APFS matches names with full Unicode case folding, not lower case: each pair is one file
    // there, so a create of the second would overwrite the first.
    assert!(names_equal("Stra\u{df}e", "Strasse"));
    assert!(names_equal(
        "\u{3bf}\u{3b4}\u{3bf}\u{3c2}",
        "\u{3bf}\u{3b4}\u{3bf}\u{3c3}"
    ));
    assert!(names_equal("Mu\u{17f}ic", "Music"));
    assert!(names_equal("\u{b5}", "\u{3bc}"));
    assert!(names_equal("\u{fb00}", "ff"));
    // Dotted capital I folds to i + combining dot, which APFS keeps apart from plain i.
    assert!(!names_equal("\u{130}x", "ix"));
}

/// The key must match what the file system treats as one name. Checked against the real temp
/// folder when it is case-insensitive (default macOS APFS); skipped elsewhere.
#[test]
fn test_names_equal_matches_the_file_system() {
    common::isolate();
    let dir = std::env::temp_dir().join(format!("qs_names_fs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("probe"), b"").unwrap();
    let case_insensitive = dir.join("PROBE").exists();
    std::fs::remove_file(dir.join("probe")).unwrap();
    if case_insensitive && cfg!(target_os = "macos") {
        let pairs = [
            ("Road Trip", "road trip"),
            ("Zo\u{eb}", "Zoe\u{308}"),
            ("Stra\u{df}e", "Strasse"),
            ("\u{3bf}\u{3b4}\u{3bf}\u{3c2}", "\u{3bf}\u{3b4}\u{3bf}\u{3c3}"),
            ("Mu\u{17f}ic", "Music"),
            ("\u{b5}", "\u{3bc}"),
            ("\u{fb00}", "ff"),
            ("\u{130}x", "ix"),
            ("Road Trip", "Road Trip 2"),
        ];
        for (a, b) in pairs {
            let file = dir.join(a);
            std::fs::write(&file, b"").unwrap();
            assert_eq!(
                dir.join(b).exists(),
                names_equal(a, b),
                "file system and names_equal disagree on {:?} / {:?}",
                a,
                b
            );
            std::fs::remove_file(&file).unwrap();
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_playlist_file_path_stays_in_dir() {
    common::isolate();
    let dir = Path::new("/home/me/.config/quickstream/playlists");
    assert_eq!(playlist_file_path(dir, "Road Trip"), Some(dir.join("Road Trip.m3u8")));
    assert_eq!(playlist_file_path(dir, "Café ☕"), Some(dir.join("Café ☕.m3u8")));
    assert_eq!(playlist_file_path(dir, "../config"), None);
    assert_eq!(playlist_file_path(dir, "../../../../etc/passwd"), None);
    assert_eq!(playlist_file_path(dir, "sub/name"), None);
    assert_eq!(playlist_file_path(dir, "/etc/passwd"), None);
}

#[test]
fn test_placeholder_metadata_does_no_io() {
    let root = common::isolate();
    let dir = root.join("placeholder");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("01 Airbag.flac");
    std::fs::write(&path, vec![0u8; 1000]).unwrap();

    let e = PlaylistEntry {
        path: path.clone(),
        duration_secs: 284,
        artist: "Radiohead".to_string(),
        title: "Airbag".to_string(),
    };
    let meta = placeholder_metadata(&e);
    assert_eq!(meta.filepath, path);
    assert_eq!(meta.filename, "01 Airbag.flac");
    assert_eq!(meta.title, "Airbag");
    assert_eq!(meta.artist, "Radiohead");
    assert_eq!(meta.format_name, "FLAC");
    assert_eq!(meta.duration_seconds, 284.0);
    // `fallback_metadata` would stat the file and report 1000 bytes; the placeholder does not.
    assert_eq!(meta.file_size, 0);

    let unknown = placeholder_metadata(&entry("/qs-test-nonexistent/x.mp3", -1, "", "x"));
    assert_eq!(unknown.duration_seconds, 0.0);
    assert_eq!(unknown.format_name, "MP3");
}

fn four_entries() -> Vec<PlaylistEntry> {
    vec![
        entry("/m/a.mp3", 10, "A", "a"),
        entry("/m/b.mp3", 20, "B", "b"),
        entry("/m/c.mp3", 30, "C", "c"),
        entry("/m/d.mp3", 40, "D", "d"),
    ]
}

fn no_cache(_: &Path) -> Option<AudioMetadata> {
    None
}

fn titles(tracks: &[AudioMetadata]) -> Vec<&str> {
    tracks.iter().map(|t| t.title.as_str()).collect()
}

#[test]
fn test_plan_queue_starts_at_selected_track() {
    common::isolate();
    let plan = plan_queue(&four_entries(), &[true, false, true, true], Some(2), &no_cache);
    assert_eq!(titles(&plan.tracks), vec!["a", "c", "d"]);
    assert_eq!(plan.start_index, Some(1));
    assert_eq!(plan.missing, 1);
}

#[test]
fn test_plan_queue_selected_missing_starts_at_next_existing() {
    common::isolate();
    let plan = plan_queue(&four_entries(), &[true, false, true, true], Some(1), &no_cache);
    assert_eq!(plan.start_index, Some(1)); // "c"
    assert_eq!(plan.tracks[1].title, "c");
}

#[test]
fn test_plan_queue_selected_missing_with_none_after_starts_at_first() {
    common::isolate();
    let plan = plan_queue(&four_entries(), &[false, true, true, false], Some(3), &no_cache);
    assert_eq!(titles(&plan.tracks), vec!["b", "c"]);
    assert_eq!(plan.start_index, Some(0));
    assert_eq!(plan.missing, 2);
}

#[test]
fn test_plan_queue_without_selection_has_no_start() {
    common::isolate();
    let plan = plan_queue(&four_entries(), &[true, true, true, true], None, &no_cache);
    assert_eq!(titles(&plan.tracks), vec!["a", "b", "c", "d"]);
    assert_eq!(plan.start_index, None);
    assert_eq!(plan.missing, 0);
}

#[test]
fn test_plan_queue_all_missing() {
    common::isolate();
    let plan = plan_queue(&four_entries(), &[false, false, false, false], Some(0), &no_cache);
    assert!(plan.tracks.is_empty());
    assert_eq!(plan.start_index, None);
    assert_eq!(plan.missing, 4);

    // An existence list shorter than the playlist counts the rest as missing.
    let plan = plan_queue(&four_entries(), &[true], None, &no_cache);
    assert_eq!(titles(&plan.tracks), vec!["a"]);
    assert_eq!(plan.missing, 3);
}

#[test]
fn test_plan_queue_prefers_cached_metadata() {
    common::isolate();
    let cached = |p: &Path| -> Option<AudioMetadata> {
        (p == Path::new("/m/b.mp3")).then(|| AudioMetadata {
            filepath: p.to_path_buf(),
            filename: "b.mp3".to_string(),
            title: "Cached B".to_string(),
            artist: "Tagged".to_string(),
            duration_seconds: 21.5,
            bitrate_kbps: 900,
            ..Default::default()
        })
    };
    let plan = plan_queue(&four_entries(), &[true, true, true, true], Some(0), &cached);
    assert_eq!(titles(&plan.tracks), vec!["a", "Cached B", "c", "d"]);
    assert_eq!(plan.tracks[1].bitrate_kbps, 900);
    assert_eq!(plan.tracks[0], placeholder_metadata(&four_entries()[0]));
}

#[test]
fn test_plan_queue_skips_non_audio_entries_as_missing() {
    common::isolate();
    let entries = vec![
        entry("/Music/a.flac", 10, "A", "a"),
        entry("/Users/x/.ssh/id_rsa", -1, "", "id_rsa"),
        entry("/Music/notes.txt", -1, "", "notes"),
        entry("/Music/b.mp3", 10, "B", "b"),
    ];
    // Every file "exists": the extension alone must keep non-audio files out of the queue.
    let plan = plan_queue(&entries, &[true; 4], Some(1), &no_cache);
    let paths: Vec<_> = plan.tracks.iter().map(|t| t.filepath.clone()).collect();
    assert_eq!(
        paths,
        vec![PathBuf::from("/Music/a.flac"), PathBuf::from("/Music/b.mp3")]
    );
    assert_eq!(plan.missing, 2);
    // The selected non-audio entry falls through to the next playable one.
    assert_eq!(plan.start_index, Some(1));
}

#[test]
fn test_validate_name_limits_utf8_bytes() {
    common::isolate();
    // 100 CJK characters are 300 bytes: too long for a file name once the temp suffix is added.
    let cjk = "曲".repeat(MAX_NAME_CHARS);
    assert_eq!(validate_name(&cjk), Err(NameError::TooManyBytes));
    // 66 CJK characters are 198 bytes: allowed. 67 are 201: refused.
    let ok = "曲".repeat(66);
    assert_eq!(validate_name(&ok), Ok(ok.clone()));
    assert_eq!(validate_name(&"曲".repeat(67)), Err(NameError::TooManyBytes));
    assert!(NameError::TooManyBytes.to_string().contains("200 bytes"));
}
