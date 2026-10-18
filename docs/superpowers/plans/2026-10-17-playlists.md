# Playlists Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hand-curated playlists in the terminal UI (issue #37): build lists track by track, store them as standard M3U8 files, play or queue them from a new `[5] Playlists` tab.

**Architecture:** A pure format module (`src/playlist.rs`) handles M3U8, names and queue planning. A `PlaylistStore` (`src/playlist_store.rs`) holds playlists in memory and writes every edit through the existing background persistence worker, which gains `delete` and `rename`. The TUI code lives in a new `src/ui/playlists.rs` (one `PlaylistsUi` field on `AppUI`) with small hooks in `src/ui/mod.rs`; all disk and share access runs on workers and reports back over mpsc channels.

**Tech Stack:** Rust 2021, ratatui + crossterm, serde/serde_json, existing `persist.rs` worker. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-17-playlists-design.md`

## Global Constraints

- Storage: `~/.config/quickstream/playlists/<name>.m3u8` via `paths::get_playlists_dir()` (honours `XDG_CONFIG_HOME`). Never write into the music library or `/Volumes/Music`.
- Format: extended M3U8, UTF-8, LF, no BOM: `#EXTM3U`, `#PLAYLIST:<name>`, `#EXTINF:<secs>,<artist> - <title>`, absolute path.
- Names: trimmed; reject empty, > 100 characters, `/`, `\`, control characters, leading `.`; compare case-insensitively.
- Rename uses the OS rename (never write-new-then-delete-old). Files that fail to load are never rewritten or renamed; their names are reserved.
- Non-blocking UI: no stat/read of library files on the UI thread; workers + mpsc only.
- Lists wrap on Up/Down; modals take input before global hotkeys; name prompts accept `q` as text and cancel only on `Esc`.
- No new crates. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -- --test-threads=1` pass after every task. Prefix cargo with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Every integration test file has `mod common;`; every `#[test]` starts with `common::isolate();`.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Deviation from the spec

Spec Section 3 says playlist queue batches use the existing `queue_add_tx` with a source tag. This plan uses a dedicated channel inside `PlaylistsUi`, applied by `src/ui/playlists.rs` through the existing `AppUI::enqueue_tracks`. The web path and its "(web)" status stay unchanged; the user-visible result (status names the playlist) is the same.

## Execution order

- Wave 1 (parallel): Task 1, Task 2.
- Wave 2: Task 3 (needs 1 and 2).
- Wave 3 (sequential, all edit `src/ui/`): Task 4 → 5 → 6 → 7.
- Task 8 (AGENTS.md + follow-up issue) runs after Task 7, or in parallel with Tasks 5–7 if its line-number checks are re-run at the end.

## Review Focus

1. Share unmounted or hung while tab 5 is open: the UI keeps rendering and accepting keys; rows show normally until a check returns (Task 7 test with an in-flight check).
2. Non-ASCII names and paths (`Café del Mar ☕`, Japanese paths): round trip unchanged; `Café` and `CAFÉ` count as the same name (Task 1 tests).
3. Hand-edited files with Windows paths or `file://` lines: parsed as entries that show as missing, no panic, file untouched (Task 1 and Task 3 tests).
4. Rapid edits (holding `J`): the file on disk ends in the same order as memory (Task 2 writer test, Task 3 store test).
5. Deleting or renaming the selected playlist: the cursor stays on a valid row and follows a renamed playlist to its sorted position (Task 5 tests).

---

### Task 1: Playlist format module (pure)

**Files:**
- Create: `src/playlist.rs`
- Modify: `src/lib.rs`. Insert `pub mod playlist;` straight after the existing line `pub mod player;` (line 8), so the module list stays alphabetical.
- Modify: `src/paths.rs`. Insert `get_playlists_dir()` straight before the existing anchor `/// Get path to the web dashboard session token (web_token, mode 0600).` / `pub fn get_web_token_path() -> PathBuf {` (line 101).
- Test: `tests/test_playlists.rs` (new)

**Interfaces:**
- Consumes:
  - `crate::metadata::AudioMetadata`. The fields used are `filepath`, `filename`, `title`, `artist`, `format_name`, `duration_seconds`, `file_size` and `bitrate_kbps`, plus `Default` and `PartialEq`. Also `AudioMetadata::display_title(&self) -> &str`.
  - `crate::paths::get_config_dir() -> PathBuf`
  - In tests: `common::isolate() -> &'static Path`
- Produces (exactly as in the contract):
  ```rust
  // src/playlist.rs
  pub const PLAYLIST_EXT: &str = "m3u8";
  pub const MAX_NAME_CHARS: usize = 100;
  #[derive(Debug, Clone, PartialEq)]
  pub struct PlaylistEntry { pub path: PathBuf, pub duration_secs: i64, pub artist: String, pub title: String }
  impl PlaylistEntry {
      pub fn from_metadata(meta: &crate::metadata::AudioMetadata) -> Self;
      pub fn from_path(path: &Path) -> Self;
      pub fn display_label(&self) -> String;
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct Playlist { pub name: String, pub entries: Vec<PlaylistEntry> }
  impl Playlist {
      pub fn new(name: &str) -> Self;
      pub fn to_m3u8(&self) -> String;
      pub fn total_duration_secs(&self) -> i64;
      pub fn contains_path(&self, path: &Path) -> bool;
  }
  pub fn parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist;
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum NameError { Empty, TooLong, Slash, Backslash, ControlChar, LeadingDot, Duplicate(String) }
  impl std::fmt::Display for NameError;
  impl std::error::Error for NameError {}   // addition inside this file, so `?` / anyhow work for later tasks
  pub fn validate_name(name: &str) -> Result<String, NameError>;
  pub fn names_equal(a: &str, b: &str) -> bool;
  pub fn playlist_file_path(dir: &Path, name: &str) -> Option<PathBuf>;
  pub fn placeholder_metadata(entry: &PlaylistEntry) -> crate::metadata::AudioMetadata;
  #[derive(Debug, Clone, PartialEq)]       // derives added inside this file, for test assertions
  pub struct QueuePlan { pub tracks: Vec<crate::metadata::AudioMetadata>, pub start_index: Option<usize>, pub missing: usize }
  pub fn plan_queue(entries: &[PlaylistEntry], exists: &[bool], selected: Option<usize>,
                    cached: &dyn Fn(&Path) -> Option<crate::metadata::AudioMetadata>) -> QueuePlan;
  // src/paths.rs
  pub fn get_playlists_dir() -> PathBuf;
  ```
  Private helpers inside `src/playlist.rs` (not exported): `strip_extinf`, `parse_extinf`, `split_label`, `whole_seconds`, `file_stem`, `single_line`.

- [ ] **Step 1: Write the failing tests**

Create `tests/test_playlists.rs`. This text is already `cargo fmt`-clean under the repo's `rustfmt.toml` (max_width 120):

```rust
//! Playlists (issue #37): the M3U8 format, playlist names and queue planning.

mod common;

use std::path::{Path, PathBuf};

use quickstream::metadata::AudioMetadata;
use quickstream::playlist::{
    names_equal, parse_m3u8, placeholder_metadata, plan_queue, playlist_file_path, validate_name, NameError, Playlist,
    PlaylistEntry, MAX_NAME_CHARS,
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
            base.join("../Music/b.flac"),
            PathBuf::from("/abs/c.ogg"),
        ]
    );
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
```

- [ ] **Step 2: Run the tests and see them fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --test test_playlists -- --test-threads=1
```

Expected: the test binary does not compile, with exactly these two errors:
```
error[E0432]: unresolved import `quickstream::playlist`
error[E0425]: cannot find function `get_playlists_dir` in module `quickstream::paths`
```

- [ ] **Step 3: Add `get_playlists_dir` and export the module**

In `src/lib.rs`, change:
```rust
pub mod player;
pub mod queue;
```
to:
```rust
pub mod player;
pub mod playlist;
pub mod queue;
```

In `src/paths.rs`, insert this straight before the existing anchor:
```rust
/// Get path to the web dashboard session token (web_token, mode 0600).
pub fn get_web_token_path() -> PathBuf {
```
Inserted code:
```rust
/// Get the playlists folder (playlists/, one `<name>.m3u8` per playlist). Not created here: the
/// background writer creates it with the first playlist it saves.
pub fn get_playlists_dir() -> PathBuf {
    get_config_dir().join("playlists")
}

```

- [ ] **Step 4: Write `src/playlist.rs`**

```rust
//! Playlists (issue #37): the extended M3U8 file format, playlist names, and turning a playlist
//! into queue tracks. Pure: nothing here reads or writes the disk, so it is safe on the UI thread.

use std::path::{Path, PathBuf};

use crate::metadata::AudioMetadata;

/// Extension of a playlist file in the playlists folder.
pub const PLAYLIST_EXT: &str = "m3u8";

/// Longest playlist name [`validate_name`] accepts, in characters (not bytes).
pub const MAX_NAME_CHARS: usize = 100;

/// One track of a playlist. The path is the source of truth for the file; artist, title and
/// duration are only what the `#EXTINF` line shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    /// Whole seconds, or -1 when unknown.
    pub duration_secs: i64,
    pub artist: String,
    pub title: String,
}

impl PlaylistEntry {
    /// Entry from metadata the app already holds (a queue row, the now-playing track or a
    /// metadata cache hit).
    pub fn from_metadata(meta: &AudioMetadata) -> Self {
        let duration_secs = if meta.duration_seconds > 0.0 {
            whole_seconds(meta.duration_seconds)
        } else {
            -1
        };
        Self {
            path: meta.filepath.clone(),
            duration_secs,
            artist: meta.artist.clone(),
            title: meta.display_title().to_string(),
        }
    }

    /// Entry for a file the app holds no metadata for. Reads nothing from disk: the title is the
    /// file stem, the artist empty and the duration unknown.
    pub fn from_path(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            duration_secs: -1,
            artist: String::new(),
            title: file_stem(path),
        }
    }

    /// `Artist - Title`, or the title alone when the artist is empty.
    pub fn display_label(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artist, self.title)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
}

impl Playlist {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            entries: Vec::new(),
        }
    }

    /// Extended M3U8 text: UTF-8, LF line endings, no BOM, an `#EXTINF` line before each path.
    pub fn to_m3u8(&self) -> String {
        let mut out = String::from("#EXTM3U\n");
        out.push_str(&format!("#PLAYLIST:{}\n", single_line(&self.name)));
        for entry in &self.entries {
            let secs = entry.duration_secs.max(-1);
            out.push_str(&format!("#EXTINF:{},{}\n", secs, single_line(&entry.display_label())));
            // M3U8 is UTF-8 text, so a path that is not valid UTF-8 is written lossily.
            out.push_str(&entry.path.to_string_lossy());
            out.push('\n');
        }
        out
    }

    /// Sum of the known durations; entries with an unknown (-1) duration are left out.
    pub fn total_duration_secs(&self) -> i64 {
        self.entries
            .iter()
            .filter(|e| e.duration_secs >= 0)
            .fold(0i64, |total, e| total.saturating_add(e.duration_secs))
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        self.entries.iter().any(|e| e.path == path)
    }
}

/// Parse extended M3U / M3U8 text, including files edited by hand or copied in from other
/// players: a leading BOM, CRLF line endings, blank lines and unknown `#` lines are accepted.
/// `name` (the file stem) is the playlist's name; a `#PLAYLIST` line is ignored. Relative paths
/// are resolved against `base_dir`, the folder holding the playlist file. Any other line is kept
/// as a path entry as written (a Windows path or a `file://` URL simply shows up as missing).
pub fn parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut playlist = Playlist::new(name);
    // The `#EXTINF` duration and text waiting for the path line they describe.
    let mut pending: Option<(i64, String)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(info) = strip_extinf(line) {
            pending = Some(parse_extinf(info));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // `join` keeps an absolute path unchanged and resolves a relative one against the folder.
        let path = base_dir.join(line);
        let (duration_secs, label) = pending.take().unwrap_or((-1, String::new()));
        let (artist, title) = split_label(&label);
        let title = if title.is_empty() { file_stem(&path) } else { title };
        playlist.entries.push(PlaylistEntry {
            path,
            duration_secs,
            artist,
            title,
        });
    }
    playlist
}

/// The text after `#EXTINF:` (tag matched case-insensitively), or `None` for any other line.
fn strip_extinf(line: &str) -> Option<&str> {
    const TAG: &str = "#EXTINF:";
    match line.get(..TAG.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(TAG) => Some(&line[TAG.len()..]),
        _ => None,
    }
}

/// `<duration>[ attributes],<text>` into (whole seconds or -1, text).
fn parse_extinf(info: &str) -> (i64, String) {
    let (head, text) = info.split_once(',').unwrap_or((info, ""));
    let secs = head
        .split_whitespace()
        .next()
        .and_then(|d| d.parse::<f64>().ok())
        .map(whole_seconds)
        .unwrap_or(-1);
    (secs, text.trim().to_string())
}

/// `<artist> - <title>` split at the first ` - `; text without one is the title alone.
fn split_label(label: &str) -> (String, String) {
    match label.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), label.to_string()),
    }
}

/// Rounded seconds, or -1 for a negative, NaN or infinite value.
fn whole_seconds(secs: f64) -> i64 {
    if secs.is_finite() && secs >= 0.0 {
        secs.round() as i64
    } else {
        -1
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Control characters (a newline in a tag, say) replaced by spaces, so one value stays on one
/// line of the file.
fn single_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Why a playlist name was refused. The `Display` text is shown in the name prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    Slash,
    Backslash,
    ControlChar,
    LeadingDot,
    /// Another playlist already has this name, ignoring case. Raised by the store, which knows
    /// the other names; [`validate_name`] never returns it.
    Duplicate(String),
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "Name can't be empty"),
            NameError::TooLong => write!(f, "Name is too long (max {} characters)", MAX_NAME_CHARS),
            NameError::Slash => write!(f, "Name can't contain /"),
            NameError::Backslash => write!(f, "Name can't contain \\"),
            NameError::ControlChar => write!(f, "Name can't contain control characters"),
            NameError::LeadingDot => write!(f, "Name can't start with ."),
            NameError::Duplicate(name) => write!(f, "\"{}\" already exists", name),
        }
    }
}

impl std::error::Error for NameError {}

/// Trim `name` and check it can be a playlist file name. Returns the trimmed name.
pub fn validate_name(name: &str) -> Result<String, NameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    if name.contains('/') {
        return Err(NameError::Slash);
    }
    if name.contains('\\') {
        return Err(NameError::Backslash);
    }
    if name.chars().any(char::is_control) {
        return Err(NameError::ControlChar);
    }
    if name.starts_with('.') {
        return Err(NameError::LeadingDot);
    }
    Ok(name.to_string())
}

/// Playlist names compare case-insensitively (Unicode lower case), because the default macOS
/// file system does: `road trip.m3u8` and `Road Trip.m3u8` are the same file there.
pub fn names_equal(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// `<dir>/<name>.m3u8`, or `None` when that file would not sit directly in `dir`. Every playlist
/// write and delete goes through this, so no playlist operation can reach the music library or
/// the other config files.
pub fn playlist_file_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{}.{}", name, PLAYLIST_EXT));
    (path.parent() == Some(dir)).then_some(path)
}

/// Queue metadata built from a playlist entry alone. Unlike `metadata::fallback_metadata` it does
/// not `stat` the file, so it never waits on a slow or unmounted share. Fields the entry does not
/// know stay empty or zero; the queue's background tag fetch resolves tracks that have no artist
/// or duration.
pub fn placeholder_metadata(entry: &PlaylistEntry) -> AudioMetadata {
    let filename = entry
        .path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string());
    let format_name = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("UNKNOWN")
        .to_uppercase();
    AudioMetadata {
        filepath: entry.path.clone(),
        filename,
        title: entry.title.clone(),
        artist: entry.artist.clone(),
        format_name,
        duration_seconds: entry.duration_secs.max(0) as f64,
        ..Default::default()
    }
}

/// What to put in the queue for a playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuePlan {
    /// The entries whose file exists, in playlist order.
    pub tracks: Vec<AudioMetadata>,
    /// Position in `tracks` to start playing from; `None` when no track was selected or no file
    /// exists.
    pub start_index: Option<usize>,
    /// Entries left out because their file is missing.
    pub missing: usize,
}

/// Keep the entries whose `exists[i]` is true (an index past the end of `exists` counts as
/// missing), using `cached` metadata when the app has it and [`placeholder_metadata`] otherwise.
/// With a `selected` entry, playback starts at that entry if it exists, else at the next existing
/// entry after it, else at the first existing entry.
pub fn plan_queue(
    entries: &[PlaylistEntry],
    exists: &[bool],
    selected: Option<usize>,
    cached: &dyn Fn(&Path) -> Option<AudioMetadata>,
) -> QueuePlan {
    let mut tracks = Vec::new();
    // Existing entries before the selected one: the queue position of the selected entry if it
    // exists, otherwise of the next existing entry after it.
    let mut kept_before_selected = 0;
    for (i, entry) in entries.iter().enumerate() {
        if !exists.get(i).copied().unwrap_or(false) {
            continue;
        }
        if selected.is_some_and(|s| i < s) {
            kept_before_selected += 1;
        }
        tracks.push(cached(&entry.path).unwrap_or_else(|| placeholder_metadata(entry)));
    }
    let start_index = match selected {
        Some(_) if kept_before_selected < tracks.len() => Some(kept_before_selected),
        Some(_) if !tracks.is_empty() => Some(0),
        _ => None,
    };
    let missing = entries.len() - tracks.len();
    QueuePlan {
        tracks,
        start_index,
        missing,
    }
}
```

- [ ] **Step 5: Run the tests and see them pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --test test_playlists -- --test-threads=1
cargo test --test test_isolation -- --test-threads=1
cargo test --lib paths -- --test-threads=1
```

Expected:
- `test_playlists`: `test result: ok. 29 passed; 0 failed`
- `test_isolation`: `test result: ok. 5 passed` (every new test calls `common::isolate()` first)
- `paths` unit tests: `test result: ok. 3 passed`

- [ ] **Step 6: Run the full suite, formatting and lints**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```

Expected:
- `cargo fmt --check` prints nothing and exits 0.
- clippy ends with `Finished` and no warnings. Keep `start_index` written as a `match`: written as `selected.and_then(|_| ...)` it triggers `clippy::unnecessary_lazy_evaluations`.
- Every test passes.

- [ ] **Step 7: Commit**

```bash
git add src/playlist.rs src/paths.rs src/lib.rs tests/test_playlists.rs
git commit -m "feat(playlists): M3U8 playlist format, name rules and queue planning

Pure module src/playlist.rs: PlaylistEntry/Playlist, extended M3U8
writer and tolerant parser (BOM, CRLF, unknown # lines, relative
paths), validate_name/names_equal, playlist_file_path confinement,
stat-free placeholder_metadata and plan_queue for missing files.
paths::get_playlists_dir() points at <config>/playlists.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```


---

### Task 2: Persistence worker delete and rename

**Files:**
- Modify: `src/persist.rs`
  - module doc comment, lines 8-9 (`//! - [\`PersistenceWorker\`] performs those writes on one background thread ...`)
  - `atomic_write` (line 32): the best-effort directory fsync at lines 51-56 (`// Persist the rename itself. ...`) moves into a new private helper `sync_parent_dir`. Behaviour does not change.
  - `enum Msg` (line 165): add the `Delete` and `Rename` variants
  - `impl PersistHandle`, `pub fn save` (line 182): update its doc comment, then add `delete` and `rename`
  - `impl PersistenceWorker`, `pub fn save` (line 236): add the `delete` and `rename` forwarders
  - `fn write_logged` (line 265) is unchanged. Below it, `struct Batch` (lines 275-299) is rewritten as an ordered `Vec<Op>`, with new `delete_logged`, `rename_logged` and `Op`.
  - `fn worker_loop`, the loop `for (path, bytes) in &batch.writes` (line 320): apply `batch.ops` in order
- Test: `tests/test_persistence_worker.rs` (append at the end of the file).
  - **Deviation from the contract, which names `tests/test_atomic_persistence.rs`:** the `PersistenceWorker` tests already live in `tests/test_persistence_worker.rs` (issue #28). That file already has the `scratch()` helper and the `use quickstream::persist::{PersistenceWorker, Writer};` import. `test_atomic_persistence.rs` covers `atomic_write` and the cache, settings and bookmarks files (issue #17). No other task touches either file.

**Interfaces:**
- Consumes: nothing from other tasks. It uses the existing `PersistHandle`, `PersistenceWorker`, `Writer`, `write_logged` and `crate::logger::log_raw`.
- Produces (Task 3's `PlaylistStore` uses these):
  ```rust
  impl PersistHandle {
      pub fn delete(&self, path: PathBuf);
      pub fn rename(&self, from: PathBuf, to: PathBuf);
  }
  impl PersistenceWorker {
      pub fn delete(&self, path: PathBuf);
      pub fn rename(&self, from: PathBuf, to: PathBuf);
  }
  // private
  enum Msg { Write(PathBuf, Vec<u8>), Delete(PathBuf), Rename(PathBuf, PathBuf), Flush(mpsc::Sender<()>), Shutdown }
  enum Op { Write(PathBuf, Vec<u8>), Delete(PathBuf), Rename(PathBuf, PathBuf) }
  ```
- Semantics:
  - Operations run in the order they were queued.
  - A save replaces an earlier queued save of the same path, and keeps that save's place, only if no delete or rename of any path was queued after it. This is stricter than the contract's "touching that path". On a case-insensitive disk, `road trip.m3u8` and `Road Trip.m3u8` are one file, so comparing paths cannot tell whether a rename touches a pending save.
  - Delete ignores `NotFound` and logs other errors. Rename uses `fs::rename` and logs errors.
  - When the worker has shut down, `delete` and `rename` run on the calling thread, as `save` does.
  - Deletes and renames always go to the file system directly. A custom `Writer` only changes how saves are written.

- [ ] **Step 1: Write the failing tests**

Append to the end of `tests/test_persistence_worker.rs`, after `test_ui_change_right_before_quit_is_flushed_on_drop`:

```rust

// ---------- deletes and renames (issue #37: playlist files) ----------

/// File names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn test_delete_removes_the_file() {
    common::isolate();
    let dir = scratch("delete");
    let path = dir.join("Road Trip.m3u8");
    std::fs::write(&path, b"#EXTM3U\n").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.delete(path.clone());
    assert!(worker.flush());
    assert!(!path.exists(), "delete removes the file");

    worker.delete(path.clone()); // already gone: nothing to do, the worker keeps running
    worker.save(dir.join("after.m3u8"), b"ok".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["after.m3u8".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_rename_moves_the_file() {
    common::isolate();
    let dir = scratch("rename");
    let from = dir.join("Road Trip.m3u8");
    let to = dir.join("Holiday.m3u8");
    std::fs::write(&from, b"#EXTM3U\n/a.flac\n").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.rename(from.clone(), to.clone());
    assert!(worker.flush());
    assert!(!from.exists(), "old name is gone");
    assert_eq!(std::fs::read(&to).unwrap(), b"#EXTM3U\n/a.flac\n");
    assert_eq!(names_in(&dir), vec!["Holiday.m3u8".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_then_delete_leaves_no_file() {
    common::isolate();
    let dir = scratch("save_delete");
    let path = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    worker.save(path.clone(), b"v1".to_vec());
    worker.delete(path.clone());
    assert!(worker.flush());
    assert!(names_in(&dir).is_empty(), "left behind: {:?}", names_in(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_delete_then_save_leaves_the_new_bytes() {
    common::isolate();
    let dir = scratch("delete_save");
    let path = dir.join("Road Trip.m3u8");
    std::fs::write(&path, b"old").unwrap();
    let worker = PersistenceWorker::spawn();

    worker.delete(path.clone());
    worker.save(path.clone(), b"new".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_after_a_delete_is_not_merged_into_the_earlier_save() {
    common::isolate();
    let dir = scratch("save_delete_save");
    let path = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    // Merging v2 into the v1 slot would write v2 and then delete it.
    worker.save(path.clone(), b"v1".to_vec());
    worker.delete(path.clone());
    worker.save(path.clone(), b"v2".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_rename_save_leaves_only_the_new_name_with_the_last_bytes() {
    common::isolate();
    let dir = scratch("save_rename_save");
    let old = dir.join("Road Trip.m3u8");
    let new = dir.join("Holiday.m3u8");
    let worker = PersistenceWorker::spawn();

    worker.save(old.clone(), b"#PLAYLIST:Road Trip\n".to_vec());
    worker.rename(old.clone(), new.clone());
    worker.save(new.clone(), b"#PLAYLIST:Holiday\n".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["Holiday.m3u8".to_string()]);
    assert_eq!(std::fs::read(&new).unwrap(), b"#PLAYLIST:Holiday\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_to_the_old_name_after_a_rename_is_not_merged_into_the_earlier_save() {
    common::isolate();
    let dir = scratch("save_rename_save_old");
    let old = dir.join("a.m3u8");
    let new = dir.join("b.m3u8");
    let worker = PersistenceWorker::spawn();

    // Merging the second save of a.m3u8 into the first would carry v2 along to b.m3u8.
    worker.save(old.clone(), b"v1".to_vec());
    worker.rename(old.clone(), new.clone());
    worker.save(old.clone(), b"v2".to_vec());
    assert!(worker.flush());
    assert_eq!(std::fs::read(&new).unwrap(), b"v1");
    assert_eq!(std::fs::read(&old).unwrap(), b"v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_case_only_rename_keeps_one_file_with_the_new_case() {
    common::isolate();
    let dir = scratch("case_rename");
    let lower = dir.join("road trip.m3u8");
    let upper = dir.join("Road Trip.m3u8");
    let worker = PersistenceWorker::spawn();

    // On a case-insensitive disk (the macOS default) these are one file: the OS rename must
    // change its case, and the save after it must not land under the old spelling.
    worker.save(lower.clone(), b"#PLAYLIST:road trip\n/a.flac\n".to_vec());
    worker.rename(lower.clone(), upper.clone());
    worker.save(upper.clone(), b"#PLAYLIST:Road Trip\n/a.flac\n".to_vec());
    assert!(worker.flush());
    assert_eq!(names_in(&dir), vec!["Road Trip.m3u8".to_string()]);
    assert_eq!(std::fs::read(&upper).unwrap(), b"#PLAYLIST:Road Trip\n/a.flac\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_many_rapid_saves_end_with_the_last_bytes() {
    common::isolate();
    let dir = scratch("rapid");
    let path = dir.join("Road Trip.m3u8");
    let other = dir.join("Holiday.m3u8");
    let worker = PersistenceWorker::spawn();

    // Holding K/J in the tracks pane: one save per keypress, interleaved with another file.
    for i in 0..200 {
        worker.save(path.clone(), format!("v{}", i).into_bytes());
        if i % 50 == 0 {
            worker.save(other.clone(), format!("o{}", i).into_bytes());
        }
    }
    assert!(worker.flush());
    assert_eq!(std::fs::read(&path).unwrap(), b"v199");
    assert_eq!(std::fs::read(&other).unwrap(), b"o150");
    assert_eq!(
        names_in(&dir),
        vec!["Holiday.m3u8".to_string(), "Road Trip.m3u8".to_string()]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_delete_and_rename_after_shutdown_run_on_the_calling_thread() {
    common::isolate();
    let dir = scratch("late_ops");
    let a = dir.join("a.m3u8");
    let b = dir.join("b.m3u8");
    let gone = dir.join("gone.m3u8");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&gone, b"x").unwrap();
    let worker = PersistenceWorker::spawn();
    let handle = worker.handle();
    drop(worker);

    handle.rename(a.clone(), b.clone());
    handle.delete(gone.clone());
    assert_eq!(names_in(&dir), vec!["b.m3u8".to_string()]);
    assert_eq!(std::fs::read(&b).unwrap(), b"a");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_persistence_worker -- --test-threads=1
```
Expected: the test file does not compile.
```
error[E0599]: no method named `delete` found for struct `PersistenceWorker` in the current scope
error[E0599]: no method named `rename` found for struct `PersistenceWorker` in the current scope
error[E0599]: no method named `rename` found for struct `PersistHandle` in the current scope
```

- [ ] **Step 3: Update the module doc and move the directory fsync into a helper**

In `src/persist.rs`, replace the module doc lines 8-9:
```rust
//! - [`PersistenceWorker`] performs those writes on one background thread so the UI thread
//!   never waits on disk; it coalesces rapid saves and flushes on drop.
```
with:
```rust
//! - [`PersistenceWorker`] performs those writes, and deletes and renames of the same files
//!   (playlists), on one background thread so the UI thread never waits on disk. It applies
//!   them in the order they were queued, coalesces rapid saves and flushes on drop.
```

At the end of `atomic_write`, replace:
```rust
    // Persist the rename itself. Best effort: not every filesystem supports fsync on a directory.
    if let Some(parent) = parent {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}
```
with:
```rust
    sync_parent_dir(path);
    Ok(())
}

/// Persist a rename or unlink in `path`'s directory. Best effort: not every filesystem supports
/// fsync on a directory.
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}
```
Earlier in `atomic_write`, `create_dir_all` still uses the local `parent`, so that variable stays.

- [ ] **Step 4: Add the message variants and the public methods**

Replace `enum Msg`:
```rust
enum Msg {
    Write(PathBuf, Vec<u8>),
    Flush(mpsc::Sender<()>),
    Shutdown,
}
```
with:
```rust
enum Msg {
    Write(PathBuf, Vec<u8>),
    Delete(PathBuf),
    Rename(PathBuf, PathBuf),
    Flush(mpsc::Sender<()>),
    Shutdown,
}
```

In `impl PersistHandle`, replace `save` and its doc comment:
```rust
    /// Queue `bytes` to replace `path`. Returns immediately; a later save of the same path that
    /// is queued before this one is written replaces it. If the worker has already shut down,
    /// the write happens on the calling thread so it is never lost.
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        if let Err(mpsc::SendError(Msg::Write(path, bytes))) = self.tx.send(Msg::Write(path, bytes)) {
            write_logged(&self.writer, &path, &bytes);
        }
    }
```
with:
```rust
    /// Queue `bytes` to replace `path`. Returns immediately; a later save of the same path that
    /// is queued before this one is written replaces it, unless a delete or rename was queued
    /// in between. If the worker has already shut down, the write happens on the calling thread
    /// so it is never lost.
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        if let Err(mpsc::SendError(Msg::Write(path, bytes))) = self.tx.send(Msg::Write(path, bytes)) {
            write_logged(&self.writer, &path, &bytes);
        }
    }

    /// Queue removing `path`, after everything queued before it. Returns immediately. A file
    /// that is already gone is not an error. If the worker has already shut down, the delete
    /// happens on the calling thread.
    pub fn delete(&self, path: PathBuf) {
        if let Err(mpsc::SendError(Msg::Delete(path))) = self.tx.send(Msg::Delete(path)) {
            delete_logged(&path);
        }
    }

    /// Queue moving `from` to `to` with the operating system's rename, after everything queued
    /// before it. Returns immediately. A case-only rename (`road trip.m3u8` to `Road Trip.m3u8`)
    /// keeps its one file on a case-insensitive disk. If the worker has already shut down, the
    /// rename happens on the calling thread.
    ///
    /// Deletes and renames always act on the file system directly; a custom [`Writer`] only
    /// replaces how saves are written.
    pub fn rename(&self, from: PathBuf, to: PathBuf) {
        if let Err(mpsc::SendError(Msg::Rename(from, to))) = self.tx.send(Msg::Rename(from, to)) {
            rename_logged(&from, &to);
        }
    }
```

In `impl PersistenceWorker`, replace:
```rust
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        self.handle.save(path, bytes)
    }

    pub fn flush(&self) -> bool {
```
with:
```rust
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        self.handle.save(path, bytes)
    }

    pub fn delete(&self, path: PathBuf) {
        self.handle.delete(path)
    }

    pub fn rename(&self, from: PathBuf, to: PathBuf) {
        self.handle.rename(from, to)
    }

    pub fn flush(&self) -> bool {
```

- [ ] **Step 5: Turn the batch into an ordered list of operations**

Replace the whole `Batch` struct and `absorb`. This starts at the doc line `/// Queued writes, at most one per path (the latest), in first-queued order.` and ends at the closing brace of `absorb`, just before `/// A flush or shutdown is waiting: ...`:
```rust
/// Queued writes, at most one per path (the latest), in first-queued order.
#[derive(Default)]
struct Batch {
    writes: Vec<(PathBuf, Vec<u8>)>,
    acks: Vec<mpsc::Sender<()>>,
    shutdown: bool,
}

impl Batch {
    fn absorb(&mut self, msg: Msg) {
        match msg {
            Msg::Write(path, bytes) => match self.writes.iter_mut().find(|(p, _)| *p == path) {
                Some(slot) => slot.1 = bytes,
                None => self.writes.push((path, bytes)),
            },
            Msg::Flush(ack) => self.acks.push(ack),
            Msg::Shutdown => self.shutdown = true,
        }
    }
```
with:
```rust
/// Remove `path`. A file that is already gone is what the caller asked for, so it is not logged.
fn delete_logged(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => crate::logger::log_raw(
            "WARN",
            "persist",
            &format!("failed to delete '{}': {}", path.display(), e),
        ),
    }
}

fn rename_logged(from: &Path, to: &Path) {
    match fs::rename(from, to) {
        Ok(()) => sync_parent_dir(to),
        Err(e) => crate::logger::log_raw(
            "WARN",
            "persist",
            &format!("failed to rename '{}' to '{}': {}", from.display(), to.display(), e),
        ),
    }
}

/// One queued file operation.
enum Op {
    Write(PathBuf, Vec<u8>),
    Delete(PathBuf),
    Rename(PathBuf, PathBuf),
}

impl Op {
    fn apply(&self, writer: &Writer) {
        match self {
            Op::Write(path, bytes) => write_logged(writer, path, bytes),
            Op::Delete(path) => delete_logged(path),
            Op::Rename(from, to) => rename_logged(from, to),
        }
    }
}

/// Queued operations in the order they were sent, with rapid saves of one path merged.
#[derive(Default)]
struct Batch {
    ops: Vec<Op>,
    acks: Vec<mpsc::Sender<()>>,
    shutdown: bool,
}

impl Batch {
    fn absorb(&mut self, msg: Msg) {
        match msg {
            Msg::Write(path, bytes) => self.queue_write(path, bytes),
            Msg::Delete(path) => self.ops.push(Op::Delete(path)),
            Msg::Rename(from, to) => self.ops.push(Op::Rename(from, to)),
            Msg::Flush(ack) => self.acks.push(ack),
            Msg::Shutdown => self.shutdown = true,
        }
    }

    /// A save replaces an earlier queued save of the same path (keeping that save's place), but
    /// never one queued before a delete or rename: moving the bytes ahead of it would let the
    /// delete remove them or the rename carry them to the new name. Any delete or rename stops
    /// the merge, not only one naming this path, because on a case-insensitive disk
    /// `road trip.m3u8` and `Road Trip.m3u8` are one file.
    fn queue_write(&mut self, path: PathBuf, bytes: Vec<u8>) {
        for op in self.ops.iter_mut().rev() {
            match op {
                Op::Write(p, queued) if *p == path => {
                    *queued = bytes;
                    return;
                }
                Op::Write(..) => {}
                Op::Delete(_) | Op::Rename(..) => break,
            }
        }
        self.ops.push(Op::Write(path, bytes));
    }
```
`fn urgent` is unchanged and stays after this block.

In `worker_loop`, replace:
```rust
        for (path, bytes) in &batch.writes {
            write_logged(writer, path, bytes);
        }
```
with:
```rust
        for op in &batch.ops {
            op.apply(writer);
        }
```

- [ ] **Step 6: Run the tests and confirm they pass**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_persistence_worker -- --test-threads=1
```
Expected: `test result: ok. 16 passed; 0 failed`. That is the 6 existing tests plus the 10 new ones. `test_save_returns_while_writer_is_busy_and_rapid_saves_coalesce` still sees `[v1, v4]`, because saves still merge when no delete or rename sits between them.

As a sanity check, temporarily change `Op::Delete(_) | Op::Rename(..) => break,` to `=> {}` and run again. `test_save_after_a_delete_is_not_merged_into_the_earlier_save` and `test_save_to_the_old_name_after_a_rename_is_not_merged_into_the_earlier_save` must fail. Then revert the change.

Then run the full suite:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1
```
Expected: every test binary reports `0 failed`.

- [ ] **Step 7: Format and lint**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo fmt --check && cargo clippy --all-targets -- -D warnings
```
Expected: no output from `fmt --check`. Clippy ends with `Finished` and no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/persist.rs tests/test_persistence_worker.rs
git commit -m "$(cat <<'EOF'
feat(persist): ordered delete and rename on the persistence worker

PersistHandle and PersistenceWorker gain delete(path) and rename(from, to)
for playlist files (#37). Saves, deletes and renames run in the order they
were queued; a save is only merged into an earlier save of the same path
when no delete or rename was queued in between, so save-delete-save and
save-rename-save end in the expected state, including case-only renames
on case-insensitive disks. Deleting a missing file is not logged. After
shutdown both run on the calling thread, like save.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```


---

### Task 3: PlaylistStore

**Files:**
- Create: `src/playlist_store.rs`
- Modify: `src/lib.rs`. Add `pub mod playlist_store;` on the line after `pub mod playlist;`. Task 1 inserts `pub mod playlist;` after `pub mod player;`, which is line 8 of the current file.
- Test: `tests/test_playlist_store.rs` (new)

**Interfaces:**
- Consumes:
  - From Task 1 (`src/playlist.rs`):
    - `pub const PLAYLIST_EXT: &str` (`"m3u8"`)
    - `pub struct PlaylistEntry { pub path: PathBuf, pub duration_secs: i64, pub artist: String, pub title: String }`
    - `pub struct Playlist { pub name: String, pub entries: Vec<PlaylistEntry> }`
    - `Playlist::new(&str) -> Playlist`
    - `Playlist::to_m3u8(&self) -> String`
    - `pub fn parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist`
    - `pub enum NameError { Empty, TooLong, Slash, Backslash, ControlChar, LeadingDot, Duplicate(String) }` and its `Display`
    - `pub fn validate_name(name: &str) -> Result<String, NameError>`
    - `pub fn names_equal(a: &str, b: &str) -> bool`
    - `pub fn playlist_file_path(dir: &Path, name: &str) -> Option<PathBuf>`
  - From Task 2 (`src/persist.rs`):
    - `PersistHandle::save(&self, PathBuf, Vec<u8>)`
    - `PersistHandle::delete(&self, PathBuf)`
    - `PersistHandle::rename(&self, PathBuf, PathBuf)`
    - `PersistenceWorker::{spawn, handle, flush}`
    - The writer applies saves, deletes and renames in the order it receives them.
  - Existing: `crate::logger::log_raw(level: &str, module: &str, message: &str)` (`src/logger.rs:313`).
- Produces (exactly the contract; nothing else is public):
  ```rust
  pub struct PlaylistStore { /* private: dir: PathBuf, playlists: Vec<Playlist>, reserved: Vec<String>, loaded: bool, persist: PersistHandle */ }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum StoreError { NotLoaded, NotFound(String), Name(crate::playlist::NameError), IndexOutOfRange, UnsafePath }
  impl std::fmt::Display for StoreError
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct AddResult { pub added: usize, pub skipped: usize, pub total: usize }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum MoveDir { Up, Down }
  impl PlaylistStore {
      pub fn new(dir: PathBuf, persist: crate::persist::PersistHandle) -> Self;
      pub fn load_dir(dir: &Path) -> Vec<crate::playlist::Playlist>; // == load_dir_reserving(dir).0
      pub fn load_dir_reserving(dir: &Path) -> (Vec<crate::playlist::Playlist>, Vec<String>); // (loaded, skipped file stems)
      pub fn set_loaded(&mut self, playlists: Vec<crate::playlist::Playlist>);
      pub fn reserve_names(&mut self, names: Vec<String>);
      pub fn is_loaded(&self) -> bool;
      pub fn dir(&self) -> &Path;
      pub fn playlists(&self) -> &[crate::playlist::Playlist];
      pub fn get(&self, name: &str) -> Option<&crate::playlist::Playlist>;
      pub fn index_of(&self, name: &str) -> Option<usize>;
      pub fn create(&mut self, name: &str) -> Result<String, StoreError>;
      pub fn add(&mut self, name: &str, entries: Vec<crate::playlist::PlaylistEntry>) -> Result<AddResult, StoreError>;
      pub fn remove(&mut self, name: &str, index: usize) -> Result<crate::playlist::PlaylistEntry, StoreError>;
      pub fn move_entry(&mut self, name: &str, index: usize, dir: MoveDir) -> Result<usize, StoreError>;
      pub fn rename(&mut self, old: &str, new: &str) -> Result<String, StoreError>;
      pub fn delete(&mut self, name: &str) -> Result<(), StoreError>;
  }
  ```
  - `NameError::Duplicate` carries the stored name of the existing playlist, so the message reads `"Road Trip" already exists`.
  - Each edit to an existing playlist checks in this order: `NotLoaded`, then `NotFound`, then `UnsafePath` (from `playlist_file_path`), then `IndexOutOfRange`. Then it changes the in-memory copy and sends the write to the persist worker.
  - `add` writes nothing when it adds 0 tracks.
  - `rename` to the exact current name changes nothing and writes nothing.
  - `load_dir_reserving` reads only files whose extension is exactly `m3u8` (lower case). It logs and skips other cases such as `Mix.M3U8`. Every file the store owns is therefore a path that `playlist_file_path` can rebuild, so `delete`, `rename` and saves always reach the file that was loaded, on case-sensitive disks too. `load_dir` keeps its signature and returns `load_dir_reserving(dir).0`, as the contract says.
  - Spec Section 1: a playlist file that is skipped at load is never rewritten or renamed. `load_dir_reserving` returns, as its second value, the file stem of every `*.m3u8` entry (any extension case) it skipped: wrong extension case, not a regular file, unreadable, or not valid UTF-8. Hidden files (stem starting with `.`) are skipped silently and not returned, because `validate_name` rejects a leading dot. A file name that is not UTF-8 is logged and not returned, because a typed name can never produce it. `reserve_names` records those stems. `create` and `rename` refuse a new name equal (`names_equal`) to a reserved one with `StoreError::Name(NameError::Duplicate(<reserved stem>))`, so on a case-insensitive disk `create("latin")` can never write `latin.m3u8` over a skipped `Latin.m3u8`, and `create("mix")` never writes over `Mix.M3U8`. `rename` skips this check when the new name equals the playlist's own current name ignoring case (a case-only rename keeps the playlist's own file). The check runs after the in-memory duplicate check and before `UnsafePath` on the new name.
  - `set_loaded` also reserves the name of every playlist it drops as a case duplicate. On a case-sensitive disk `mix.m3u8` and `Mix.m3u8` can both exist; the store keeps `Mix` and drops `mix`, whose file is never rewritten. So `create("mix")` after `delete("Mix")` is refused by the reserved check, and `rename` refuses, even for a case-only rename, a new name byte-equal to a reserved stem (`rename("Mix", "mix")` returns `Err(Name(Duplicate("mix")))` instead of letting `fs::rename(Mix.m3u8, mix.m3u8)` replace the dropped file). This byte-equal check runs right after the reserved check.
  - The caller (Task 4) runs `load_dir_reserving` on its worker, then calls `set_loaded(playlists)` and `reserve_names(skipped)`. The tests' `open_store` helper does the same.

- [ ] **Step 1: Write the failing tests (load, sort, create, add)**

Create `tests/test_playlist_store.rs`:

```rust
//! Issue #37: `PlaylistStore` keeps the playlists in memory, sorted by name, and writes every
//! edit through the background writer to `<playlists dir>/<name>.m3u8`.
//!
//! Every test uses its own temp folder as the playlists dir and a real `PersistenceWorker`;
//! nothing here touches the user's real config.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use quickstream::persist::PersistenceWorker;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::{AddResult, PlaylistStore, StoreError};

/// A fresh, empty parent folder; the playlists dir inside it does not exist yet.
fn scratch(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("qs_playlist_store_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    base.join("playlists")
}

/// A loaded store over `dir` backed by a real writer, set up the way the UI does at startup:
/// load, then reserve the names of skipped files. Keep the worker alive for the whole test.
fn open_store(dir: &Path) -> (PersistenceWorker, PlaylistStore) {
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir.to_path_buf(), worker.handle());
    let (playlists, skipped) = PlaylistStore::load_dir_reserving(dir);
    store.set_loaded(playlists);
    store.reserve_names(skipped);
    (worker, store)
}

fn entry(name: &str) -> PlaylistEntry {
    PlaylistEntry {
        path: PathBuf::from(format!("/nonexistent/qs-test/{}.flac", name)),
        duration_secs: 200,
        artist: "Artist".to_string(),
        title: name.to_string(),
    }
}

fn paths(entries: &[PlaylistEntry]) -> Vec<PathBuf> {
    entries.iter().map(|e| e.path.clone()).collect()
}

fn names(store: &PlaylistStore) -> Vec<&str> {
    store.playlists().iter().map(|p| p.name.as_str()).collect()
}

/// File names in `dir`, sorted, exactly as `read_dir` reports them (case included).
fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// The playlist called exactly `name` as a fresh load of `dir` sees it on disk.
fn on_disk(dir: &Path, name: &str) -> Playlist {
    PlaylistStore::load_dir(dir)
        .into_iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no file for {:?} in {:?}", name, file_names(dir)))
}

#[test]
fn test_edits_are_refused_until_loaded() {
    common::isolate();
    let dir = scratch("not_loaded");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir.clone(), worker.handle());
    assert!(!store.is_loaded());
    assert_eq!(store.dir(), dir.as_path());
    assert_eq!(store.create("Road Trip"), Err(StoreError::NotLoaded));
    assert_eq!(store.add("Road Trip", vec![entry("a")]), Err(StoreError::NotLoaded));
    assert_eq!(StoreError::NotLoaded.to_string(), "Playlists are still loading");
    assert!(worker.flush());
    assert!(!dir.exists(), "a refused edit must not create the playlists dir");

    store.set_loaded(Vec::new());
    assert!(store.is_loaded());
    assert_eq!(store.create("Road Trip"), Ok("Road Trip".to_string()));
}

#[test]
fn test_load_dir_of_missing_folder_is_empty_and_creates_nothing() {
    common::isolate();
    let dir = scratch("missing");
    assert!(PlaylistStore::load_dir(&dir).is_empty());
    assert_eq!(PlaylistStore::load_dir_reserving(&dir), (Vec::new(), Vec::new()));
    assert!(!dir.exists());
}

#[test]
fn test_load_dir_reserving_returns_the_stems_it_skipped() {
    common::isolate();
    let dir = scratch("reserving");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Good.m3u8"), "#EXTM3U\n/music/good.flac\n").unwrap();
    fs::write(dir.join("Latin.m3u8"), b"#EXTM3U\n/music/caf\xe9.flac\n").unwrap();
    fs::write(dir.join("Mix.M3U8"), "#EXTM3U\n/music/mix.flac\n").unwrap();
    fs::create_dir_all(dir.join("folder.m3u8")).unwrap();
    // Hidden and non-playlist files are neither loaded nor reserved.
    fs::write(dir.join("._Good.m3u8"), [0u8, 5, 22, 7]).unwrap();
    fs::write(dir.join("notes.txt"), "/music/not-a-playlist.flac\n").unwrap();

    let (loaded, skipped) = PlaylistStore::load_dir_reserving(&dir);
    let loaded_names: Vec<&str> = loaded.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(loaded_names, vec!["Good"]);
    assert_eq!(skipped, vec!["Latin", "Mix", "folder"]);
    assert_eq!(PlaylistStore::load_dir(&dir), loaded);
}

#[test]
fn test_load_dir_reads_only_lower_case_m3u8_files() {
    common::isolate();
    let dir = scratch("load");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("Road Trip.m3u8"),
        "#EXTM3U\n#EXTINF:284,Radiohead - Airbag\n/music/airbag.flac\n",
    )
    .unwrap();
    // Every write goes to `<name>.m3u8`, so on a case-sensitive disk the store could never
    // rewrite, rename or delete `Loud.M3U8`. It is skipped and left alone.
    let loud = "#EXTM3U\n/music/loud.flac\n";
    fs::write(dir.join("Loud.M3U8"), loud).unwrap();
    fs::write(dir.join("notes.txt"), "/music/not-a-playlist.flac\n").unwrap();
    fs::write(dir.join("old.m3u"), "/music/old.flac\n").unwrap();
    // macOS AppleDouble companion: hidden, not a playlist.
    fs::write(dir.join("._Road Trip.m3u8"), [0u8, 5, 22, 7]).unwrap();
    fs::create_dir_all(dir.join("folder.m3u8")).unwrap();

    let loaded = PlaylistStore::load_dir(&dir);
    let loaded_names: Vec<&str> = loaded.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(loaded_names, vec!["Road Trip"]);
    assert_eq!(paths(&loaded[0].entries), vec![PathBuf::from("/music/airbag.flac")]);
    assert_eq!(loaded[0].entries[0].artist, "Radiohead");
    assert_eq!(loaded[0].entries[0].duration_secs, 284);

    // Edits to other playlists leave the skipped file exactly as it was.
    let (worker, mut store) = open_store(&dir);
    store.add("Road Trip", vec![entry("a")]).unwrap();
    store.create("Other").unwrap();
    assert!(worker.flush());
    assert_eq!(fs::read_to_string(dir.join("Loud.M3U8")).unwrap(), loud);
    assert_eq!(
        file_names(&dir),
        vec![
            "._Road Trip.m3u8",
            "Loud.M3U8",
            "Other.m3u8",
            "Road Trip.m3u8",
            "folder.m3u8",
            "notes.txt",
            "old.m3u",
        ]
    );
}

#[test]
fn test_set_loaded_sorts_by_name_ignoring_case() {
    common::isolate();
    let dir = scratch("sort");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir, worker.handle());
    store.set_loaded(vec![
        Playlist::new("charlie"),
        Playlist::new("Bravo"),
        Playlist::new("alpha"),
    ]);
    assert_eq!(names(&store), vec!["alpha", "Bravo", "charlie"]);
    assert_eq!(store.index_of("BRAVO"), Some(1));
    assert_eq!(store.get("ALPHA").map(|p| p.name.as_str()), Some("alpha"));
    assert!(store.get("delta").is_none());
    assert_eq!(store.index_of("delta"), None);
}

#[test]
fn test_set_loaded_keeps_one_of_names_differing_only_in_case() {
    common::isolate();
    // Possible on a case-sensitive disk (Linux): `mix.m3u8` and `Mix.m3u8` side by side.
    let dir = scratch("case_dup");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir, worker.handle());
    store.set_loaded(vec![Playlist::new("mix"), Playlist::new("Mix")]);
    assert_eq!(names(&store), vec!["Mix"]);
}

#[test]
fn test_create_writes_an_empty_playlist_file() {
    common::isolate();
    let dir = scratch("create");
    let (worker, mut store) = open_store(&dir);
    assert_eq!(store.create("  Road Trip "), Ok("Road Trip".to_string()));
    assert_eq!(store.create("Chill"), Ok("Chill".to_string()));
    assert_eq!(names(&store), vec!["Chill", "Road Trip"]);
    assert!(store.get("Chill").unwrap().entries.is_empty());

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Chill.m3u8", "Road Trip.m3u8"]);
    let text = fs::read_to_string(dir.join("Road Trip.m3u8")).unwrap();
    assert!(text.starts_with("#EXTM3U\n#PLAYLIST:Road Trip\n"), "{}", text);
    assert!(on_disk(&dir, "Road Trip").entries.is_empty());
}

#[test]
fn test_create_rejects_invalid_and_duplicate_names() {
    common::isolate();
    let dir = scratch("create_reject");
    let (worker, mut store) = open_store(&dir);
    store.create("Road Trip").unwrap();
    assert_eq!(
        store.create("road trip"),
        Err(StoreError::Name(NameError::Duplicate("Road Trip".to_string())))
    );
    assert_eq!(store.create("ROAD TRIP").unwrap_err().to_string(), "\"Road Trip\" already exists");
    assert_eq!(store.create("   "), Err(StoreError::Name(NameError::Empty)));
    assert_eq!(store.create("a/b"), Err(StoreError::Name(NameError::Slash)));
    assert_eq!(store.create("a/b").unwrap_err().to_string(), "Name can't contain /");
    assert_eq!(names(&store), vec!["Road Trip"]);

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Road Trip.m3u8"]);
}

#[test]
fn test_create_refuses_the_name_of_a_skipped_file() {
    common::isolate();
    // Spec Section 1: a skipped file is never rewritten. On a case-insensitive disk (the macOS
    // default) `latin.m3u8` is `Latin.m3u8` and `mix.m3u8` is `Mix.M3U8`, so creating either
    // name would write over the skipped file.
    let dir = scratch("create_reserved");
    fs::create_dir_all(&dir).unwrap();
    let latin1: &[u8] = b"#EXTM3U\n/music/caf\xe9.flac\n";
    fs::write(dir.join("Latin.m3u8"), latin1).unwrap();
    let mix = "#EXTM3U\n/music/mix.flac\n";
    fs::write(dir.join("Mix.M3U8"), mix).unwrap();

    let (worker, mut store) = open_store(&dir);
    assert!(store.playlists().is_empty());
    assert_eq!(
        store.create("latin"),
        Err(StoreError::Name(NameError::Duplicate("Latin".to_string())))
    );
    assert_eq!(store.create(" LATIN ").unwrap_err().to_string(), "\"Latin\" already exists");
    assert_eq!(
        store.create("mix"),
        Err(StoreError::Name(NameError::Duplicate("Mix".to_string())))
    );
    assert!(store.playlists().is_empty());
    // Other names are still fine.
    assert_eq!(store.create("Latin Jazz"), Ok("Latin Jazz".to_string()));

    assert!(worker.flush());
    assert_eq!(fs::read(dir.join("Latin.m3u8")).unwrap(), latin1);
    assert_eq!(fs::read_to_string(dir.join("Mix.M3U8")).unwrap(), mix);
    assert_eq!(file_names(&dir), vec!["Latin Jazz.m3u8", "Latin.m3u8", "Mix.M3U8"]);
}

#[test]
fn test_add_skips_paths_already_in_the_playlist() {
    common::isolate();
    let dir = scratch("add");
    let (worker, mut store) = open_store(&dir);
    store.create("Road Trip").unwrap();
    assert_eq!(
        store.add("road trip", vec![entry("a"), entry("b"), entry("a")]),
        Ok(AddResult { added: 2, skipped: 1, total: 2 })
    );
    assert_eq!(
        store.add("Road Trip", vec![entry("b"), entry("c")]),
        Ok(AddResult { added: 1, skipped: 1, total: 3 })
    );
    assert_eq!(
        store.add("Road Trip", vec![entry("c")]),
        Ok(AddResult { added: 0, skipped: 1, total: 3 })
    );
    assert_eq!(
        store.add("Nope", vec![entry("a")]),
        Err(StoreError::NotFound("Nope".to_string()))
    );
    assert_eq!(
        StoreError::NotFound("Nope".to_string()).to_string(),
        "No playlist named \"Nope\""
    );

    let expected = vec![entry("a"), entry("b"), entry("c")];
    assert_eq!(store.get("Road Trip").unwrap().entries, expected);
    assert!(worker.flush());
    assert_eq!(on_disk(&dir, "Road Trip").entries, expected);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlist_store -- --test-threads=1`

Expected: the build fails with ``error[E0432]: unresolved import `quickstream::playlist_store` `` because the crate root has no module named `playlist_store`.

- [ ] **Step 3: Export the module**

In `src/lib.rs`, add `pub mod playlist_store;` on the line after `pub mod playlist;`. After Task 1 and this step, the block reads:

```rust
pub mod persist;
pub mod player;
pub mod playlist;
pub mod playlist_store;
pub mod queue;
```

- [ ] **Step 4: Write the store (types, load, lookups, create, add)**

Create `src/playlist_store.rs`:

```rust
//! In-memory playlists, written through to `<playlists dir>/<name>.m3u8` (issue #37).
//!
//! The UI thread holds the store, and the store never touches the disk on that thread:
//! [`PlaylistStore::load_dir_reserving`] runs once on a worker at startup. Every edit changes
//! the in-memory copy, then hands the whole file to the background writer ([`PersistHandle`]),
//! which applies saves, deletes and renames in the order they were sent. Every path is built by
//! [`playlist_file_path`], so no playlist operation can write outside the playlists folder.
//! Files skipped at load keep their names reserved, so no edit ever writes over one.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::persist::PersistHandle;
use crate::playlist::{
    names_equal, parse_m3u8, playlist_file_path, validate_name, NameError, Playlist, PlaylistEntry, PLAYLIST_EXT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The startup load hasn't finished; an edit now could overwrite a file not yet read.
    NotLoaded,
    NotFound(String),
    Name(NameError),
    IndexOutOfRange,
    /// The playlist's file would land outside the playlists folder.
    UnsafePath,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotLoaded => write!(f, "Playlists are still loading"),
            StoreError::NotFound(name) => write!(f, "No playlist named \"{}\"", name),
            StoreError::Name(e) => write!(f, "{}", e),
            StoreError::IndexOutOfRange => write!(f, "No such track"),
            StoreError::UnsafePath => write!(f, "Refusing to write outside the playlists folder"),
        }
    }
}

/// Outcome of [`PlaylistStore::add`]: tracks added, tracks skipped because their path was
/// already in the playlist, and the playlist's length afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddResult {
    pub added: usize,
    pub skipped: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDir {
    Up,
    Down,
}

pub struct PlaylistStore {
    dir: PathBuf,
    /// Sorted by name ignoring case; no two names are equal ignoring case.
    playlists: Vec<Playlist>,
    /// Stems of playlist files skipped at load. `create` and `rename` refuse these names.
    reserved: Vec<String>,
    loaded: bool,
    persist: PersistHandle,
}

impl PlaylistStore {
    /// An empty store over `dir` that refuses edits until [`set_loaded`](Self::set_loaded).
    pub fn new(dir: PathBuf, persist: PersistHandle) -> Self {
        Self {
            dir,
            playlists: Vec::new(),
            reserved: Vec::new(),
            loaded: false,
            persist,
        }
    }

    /// The playlists in `dir`, without the names of skipped files. Blocking: call it on a
    /// worker. See [`load_dir_reserving`](Self::load_dir_reserving).
    pub fn load_dir(dir: &Path) -> Vec<Playlist> {
        Self::load_dir_reserving(dir).0
    }

    /// Read every `*.m3u8` file directly inside `dir`. Blocking: call it on a worker. Returns
    /// the playlists and the file stems that were skipped. A playlist's name is its file stem.
    /// Only the exact lower-case extension is read: every write goes to `<name>.m3u8`, so on a
    /// case-sensitive disk the store could never rewrite, rename or delete a `Mix.M3U8`. Such
    /// files are logged and skipped, and so are entries that aren't regular files, files that
    /// can't be read and files that aren't UTF-8. Their stems come back as the second value;
    /// hand them to [`reserve_names`](Self::reserve_names) so no edit writes over them. Hidden
    /// files (macOS `._Road Trip.m3u8`) are skipped silently and not returned, because no valid
    /// name starts with a dot. Never writes; a missing folder gives two empty lists.
    pub fn load_dir_reserving(dir: &Path) -> (Vec<Playlist>, Vec<String>) {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
            Err(e) => {
                log_warn(&format!("could not list playlists folder '{}': {}", dir.display(), e));
                return (Vec::new(), Vec::new());
            }
        };
        let mut files: Vec<PathBuf> = listing
            .flatten()
            .map(|e| e.path())
            .filter(|p| has_playlist_extension(p))
            .collect();
        files.sort();

        let mut playlists = Vec::new();
        let mut skipped = Vec::new();
        for path in files {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                // A typed name is always UTF-8 and can never reach this file: nothing to reserve.
                log_warn(&format!("skipping playlist with a non-UTF-8 file name: '{}'", path.display()));
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match read_playlist_text(&path) {
                Ok(text) => playlists.push(parse_m3u8(name, &text, dir)),
                Err(reason) => {
                    log_warn(&format!("skipping playlist '{}': {}", path.display(), reason));
                    skipped.push(name.to_string());
                }
            }
        }
        (playlists, skipped)
    }

    /// Install the playlists read by [`load_dir_reserving`](Self::load_dir_reserving) and allow
    /// edits. When names differ only in case (possible on a case-sensitive disk), the first in
    /// sort order is kept and the rest are logged and ignored. The ignored names are reserved,
    /// so no create or rename ever writes over their files.
    pub fn set_loaded(&mut self, playlists: Vec<Playlist>) {
        let mut playlists = playlists;
        sort_by_name(&mut playlists);
        self.playlists = Vec::with_capacity(playlists.len());
        for playlist in playlists {
            if self
                .playlists
                .last()
                .is_some_and(|kept| names_equal(&kept.name, &playlist.name))
            {
                log_warn(&format!(
                    "ignoring playlist \"{}\": another playlist has the same name in a different case",
                    playlist.name
                ));
                // Its file is still on disk and is never rewritten (spec Section 1).
                self.reserved.push(playlist.name);
                continue;
            }
            self.playlists.push(playlist);
        }
        self.loaded = true;
    }

    /// Treat `names` (the stems [`load_dir_reserving`](Self::load_dir_reserving) skipped) as
    /// taken: [`create`](Self::create) and [`rename`](Self::rename) refuse them, ignoring case.
    /// On a case-insensitive disk `latin.m3u8` is the skipped `Latin.m3u8`, so writing it would
    /// overwrite a file the store never read.
    pub fn reserve_names(&mut self, names: Vec<String>) {
        self.reserved.extend(names);
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    /// The playlist named `name`, ignoring case.
    pub fn get(&self, name: &str) -> Option<&Playlist> {
        self.index_of(name).map(|i| &self.playlists[i])
    }

    /// Position of the playlist named `name` (ignoring case) in [`playlists`](Self::playlists).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.playlists.iter().position(|p| names_equal(&p.name, name))
    }

    /// Create an empty playlist and write its file. Returns the stored (trimmed) name.
    pub fn create(&mut self, name: &str) -> Result<String, StoreError> {
        self.ensure_loaded()?;
        let name = validate_name(name).map_err(StoreError::Name)?;
        if let Some(existing) = self.get(&name) {
            return Err(StoreError::Name(NameError::Duplicate(existing.name.clone())));
        }
        self.ensure_not_reserved(&name)?;
        let path = self.file_for(&name)?;
        let playlist = Playlist::new(&name);
        let bytes = playlist.to_m3u8().into_bytes();
        self.playlists.push(playlist);
        sort_by_name(&mut self.playlists);
        self.persist.save(path, bytes);
        Ok(name)
    }

    /// Append `entries` to the playlist named `name`, skipping any whose path is already in it
    /// (including repeats within `entries`). Writes the file only when something was added.
    pub fn add(&mut self, name: &str, entries: Vec<PlaylistEntry>) -> Result<AddResult, StoreError> {
        let (slot, path) = self.locate(name)?;
        let playlist = &mut self.playlists[slot];
        let mut present: HashSet<PathBuf> = playlist.entries.iter().map(|e| e.path.clone()).collect();
        let (mut added, mut skipped) = (0, 0);
        for entry in entries {
            if present.insert(entry.path.clone()) {
                playlist.entries.push(entry);
                added += 1;
            } else {
                skipped += 1;
            }
        }
        let total = playlist.entries.len();
        if added > 0 {
            self.write_through(slot, path);
        }
        Ok(AddResult { added, skipped, total })
    }

    fn ensure_loaded(&self) -> Result<(), StoreError> {
        if self.loaded {
            Ok(())
        } else {
            Err(StoreError::NotLoaded)
        }
    }

    /// Refuse `name` when it is the name of a file skipped at load, ignoring case. The error
    /// carries the file's own stem, so the message reads `"Latin" already exists`.
    fn ensure_not_reserved(&self, name: &str) -> Result<(), StoreError> {
        match self.reserved.iter().find(|r| names_equal(r, name)) {
            Some(reserved) => Err(StoreError::Name(NameError::Duplicate(reserved.clone()))),
            None => Ok(()),
        }
    }

    /// The file for playlist `name`, refused when it would land outside the playlists folder.
    fn file_for(&self, name: &str) -> Result<PathBuf, StoreError> {
        playlist_file_path(&self.dir, name).ok_or(StoreError::UnsafePath)
    }

    /// For an edit of an existing playlist: its position and its file, checked in contract
    /// order (loaded, exists, safe path).
    fn locate(&self, name: &str) -> Result<(usize, PathBuf), StoreError> {
        self.ensure_loaded()?;
        let slot = self
            .index_of(name)
            .ok_or_else(|| StoreError::NotFound(name.to_string()))?;
        let path = self.file_for(&self.playlists[slot].name)?;
        Ok((slot, path))
    }

    /// Hand the playlist at `slot` to the background writer as the whole new file.
    fn write_through(&self, slot: usize, path: PathBuf) {
        self.persist.save(path, self.playlists[slot].to_m3u8().into_bytes());
    }
}

/// True when the extension is `m3u8` in any case. [`PlaylistStore::load_dir_reserving`] then
/// reads only the exact lower-case extension and logs and reserves the others, so the user can
/// see why a file is missing and no edit writes over it.
fn has_playlist_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PLAYLIST_EXT))
}

/// The text of the playlist file at `path`, or why it can't be loaded.
fn read_playlist_text(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err("not a regular file".to_string());
    }
    if path.extension() != Some(OsStr::new(PLAYLIST_EXT)) {
        return Err(format!("rename it to end in .{} to use it", PLAYLIST_EXT));
    }
    let bytes = fs::read(path).map_err(|e| format!("could not read it: {}", e))?;
    String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())
}

/// Case-insensitive name order; the exact name breaks ties so the order is deterministic.
fn sort_by_name(playlists: &mut [Playlist]) {
    playlists.sort_by_cached_key(|p| (p.name.to_lowercase(), p.name.clone()));
}

fn log_warn(message: &str) {
    crate::logger::log_raw("WARN", "playlists", message);
}
```

- [ ] **Step 5: Run the tests and confirm they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlist_store -- --test-threads=1`

Expected: `test result: ok. 10 passed; 0 failed`.

- [ ] **Step 6: Write the failing tests (remove, move, rename, delete, unsafe paths, skipped files)**

In `tests/test_playlist_store.rs`, replace this import block:

```rust
use std::fs;
use std::path::{Path, PathBuf};

use quickstream::persist::PersistenceWorker;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::{AddResult, PlaylistStore, StoreError};
```

with this one:

```rust
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use quickstream::persist::PersistenceWorker;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::{AddResult, MoveDir, PlaylistStore, StoreError};
```

Append to the end of the file:

```rust
#[test]
fn test_remove_takes_out_one_track() {
    common::isolate();
    let dir = scratch("remove");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    store.add("Mix", vec![entry("a"), entry("b"), entry("c")]).unwrap();
    assert_eq!(store.remove("mix", 1), Ok(entry("b")));
    assert_eq!(store.remove("Mix", 2), Err(StoreError::IndexOutOfRange));
    assert_eq!(StoreError::IndexOutOfRange.to_string(), "No such track");
    assert_eq!(store.remove("Other", 0), Err(StoreError::NotFound("Other".to_string())));

    let expected = vec![entry("a"), entry("c")];
    assert_eq!(store.get("Mix").unwrap().entries, expected);
    assert!(worker.flush());
    assert_eq!(on_disk(&dir, "Mix").entries, expected);
}

#[test]
fn test_move_entry_moves_one_step_and_stops_at_the_edges() {
    common::isolate();
    let dir = scratch("move");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    store.add("Mix", vec![entry("a"), entry("b"), entry("c")]).unwrap();
    // No wraparound for moves: at an edge the index stays put.
    assert_eq!(store.move_entry("Mix", 0, MoveDir::Up), Ok(0));
    assert_eq!(store.move_entry("Mix", 2, MoveDir::Down), Ok(2));
    assert_eq!(store.move_entry("Mix", 0, MoveDir::Down), Ok(1)); // b a c
    assert_eq!(store.move_entry("mix", 2, MoveDir::Up), Ok(1)); // b c a
    assert_eq!(store.move_entry("Mix", 3, MoveDir::Up), Err(StoreError::IndexOutOfRange));
    assert_eq!(
        store.move_entry("Other", 0, MoveDir::Up),
        Err(StoreError::NotFound("Other".to_string()))
    );

    let expected = vec![entry("b"), entry("c"), entry("a")];
    assert_eq!(store.get("Mix").unwrap().entries, expected);
    assert!(worker.flush());
    assert_eq!(on_disk(&dir, "Mix").entries, expected);
}

#[test]
fn test_rapid_moves_leave_the_file_matching_memory() {
    common::isolate();
    // Review Focus 4: twenty moves inside the writer's coalescing window must leave the file in
    // the same order as the in-memory playlist.
    let dir = scratch("rapid_moves");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    let tracks: Vec<PlaylistEntry> = (0..10).map(|i| entry(&format!("t{}", i))).collect();
    store.add("Mix", tracks.clone()).unwrap();

    let mut index = 0;
    for step in 0..20 {
        let direction = if step % 7 == 6 { MoveDir::Up } else { MoveDir::Down };
        index = store.move_entry("Mix", index, direction).unwrap();
    }
    let in_memory = store.get("Mix").unwrap().entries.clone();
    assert_ne!(in_memory, tracks, "the moves should have changed the order");

    assert!(worker.flush());
    assert_eq!(on_disk(&dir, "Mix").entries, in_memory);
    assert_eq!(file_names(&dir), vec!["Mix.m3u8"]);
}

#[test]
fn test_rename_moves_the_file_and_rewrites_the_playlist_line() {
    common::isolate();
    let dir = scratch("rename");
    let (worker, mut store) = open_store(&dir);
    store.create("Alpha Mix").unwrap();
    store.create("Middle").unwrap();
    store.add("Alpha Mix", vec![entry("a"), entry("b")]).unwrap();
    assert!(worker.flush());

    assert_eq!(store.rename("alpha mix", " Zed Mix "), Ok("Zed Mix".to_string()));
    assert_eq!(names(&store), vec!["Middle", "Zed Mix"]);
    assert!(store.get("Alpha Mix").is_none());
    // Renaming to the exact current name changes nothing.
    assert_eq!(store.rename("Zed Mix", "Zed Mix"), Ok("Zed Mix".to_string()));

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Middle.m3u8", "Zed Mix.m3u8"]);
    let text = fs::read_to_string(dir.join("Zed Mix.m3u8")).unwrap();
    assert!(text.contains("#PLAYLIST:Zed Mix\n"), "{}", text);
    assert_eq!(on_disk(&dir, "Zed Mix").entries, vec![entry("a"), entry("b")]);
}

#[test]
fn test_rename_rejects_another_playlists_name_and_bad_names() {
    common::isolate();
    let dir = scratch("rename_reject");
    let (worker, mut store) = open_store(&dir);
    store.create("A").unwrap();
    store.create("B").unwrap();
    assert_eq!(
        store.rename("A", "b"),
        Err(StoreError::Name(NameError::Duplicate("B".to_string())))
    );
    assert_eq!(store.rename("A", "x/y"), Err(StoreError::Name(NameError::Slash)));
    assert_eq!(store.rename("Missing", "C"), Err(StoreError::NotFound("Missing".to_string())));
    assert_eq!(names(&store), vec!["A", "B"]);

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["A.m3u8", "B.m3u8"]);
}

#[test]
fn test_rename_refuses_the_name_of_a_skipped_file() {
    common::isolate();
    // Spec Section 1: renaming onto a skipped file's name would replace it on a
    // case-insensitive disk (`LATIN.m3u8` is `Latin.m3u8`, `mix.m3u8` is `Mix.M3U8`).
    let dir = scratch("rename_reserved");
    fs::create_dir_all(&dir).unwrap();
    let latin1: &[u8] = b"#EXTM3U\n/music/caf\xe9.flac\n";
    fs::write(dir.join("Latin.m3u8"), latin1).unwrap();
    let mix = "#EXTM3U\n/music/mix.flac\n";
    fs::write(dir.join("Mix.M3U8"), mix).unwrap();

    let (worker, mut store) = open_store(&dir);
    store.create("Other").unwrap();
    store.add("Other", vec![entry("a")]).unwrap();
    assert_eq!(
        store.rename("Other", "LATIN"),
        Err(StoreError::Name(NameError::Duplicate("Latin".to_string())))
    );
    assert_eq!(
        store.rename("other", "mix"),
        Err(StoreError::Name(NameError::Duplicate("Mix".to_string())))
    );
    assert_eq!(names(&store), vec!["Other"]);

    assert!(worker.flush());
    assert_eq!(fs::read(dir.join("Latin.m3u8")).unwrap(), latin1);
    assert_eq!(fs::read_to_string(dir.join("Mix.M3U8")).unwrap(), mix);
    assert_eq!(file_names(&dir), vec!["Latin.m3u8", "Mix.M3U8", "Other.m3u8"]);
    assert_eq!(on_disk(&dir, "Other").entries, vec![entry("a")]);
}

#[test]
fn test_a_case_duplicate_dropped_at_load_is_never_written_over() {
    common::isolate();
    // On a case-sensitive disk (Linux) `mix.m3u8` and `Mix.m3u8` can sit side by side.
    // `set_loaded` keeps `Mix` and drops `mix`. Spec Section 1: the dropped file is never
    // rewritten, so neither a case-only rename onto it nor delete-then-create may write it.
    let dir = scratch("case_dup_reserved");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir.clone(), worker.handle());
    store.set_loaded(vec![Playlist::new("mix"), Playlist::new("Mix")]);
    assert_eq!(names(&store), vec!["Mix"]);

    assert_eq!(
        store.rename("Mix", "mix"),
        Err(StoreError::Name(NameError::Duplicate("mix".to_string())))
    );
    assert_eq!(names(&store), vec!["Mix"]);

    store.delete("Mix").unwrap();
    assert_eq!(
        store.create("mix"),
        Err(StoreError::Name(NameError::Duplicate("mix".to_string())))
    );
    assert!(store.playlists().is_empty());

    assert!(worker.flush());
    assert!(!dir.join("mix.m3u8").exists());
}

#[test]
fn test_case_only_rename_leaves_exactly_one_file_with_the_new_case() {
    common::isolate();
    let dir = scratch("case_rename");
    let (worker, mut store) = open_store(&dir);
    store.create("road trip").unwrap();
    store.add("road trip", vec![entry("a"), entry("b"), entry("c")]).unwrap();
    assert!(worker.flush());

    assert_eq!(store.rename("road trip", "Road Trip"), Ok("Road Trip".to_string()));
    assert_eq!(names(&store), vec!["Road Trip"]);

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Road Trip.m3u8"]);
    assert_eq!(
        on_disk(&dir, "Road Trip").entries,
        vec![entry("a"), entry("b"), entry("c")]
    );
}

#[test]
fn test_rename_right_after_create_ends_with_one_file() {
    common::isolate();
    // Save, rename and save of one playlist queued together, before the writer runs.
    let dir = scratch("rename_unflushed");
    let (worker, mut store) = open_store(&dir);
    store.create("road trip").unwrap();
    store.add("road trip", vec![entry("a")]).unwrap();
    store.rename("road trip", "Road Trip").unwrap();

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Road Trip.m3u8"]);
    assert_eq!(on_disk(&dir, "Road Trip").entries, vec![entry("a")]);
}

#[test]
fn test_delete_removes_the_playlist_and_its_file() {
    common::isolate();
    let dir = scratch("delete");
    let (worker, mut store) = open_store(&dir);
    store.create("Keep").unwrap();
    store.create("Gone").unwrap();
    store.add("Gone", vec![entry("a")]).unwrap();
    assert!(worker.flush());

    assert_eq!(store.delete("gone"), Ok(()));
    assert_eq!(names(&store), vec!["Keep"]);
    assert_eq!(store.delete("Gone"), Err(StoreError::NotFound("Gone".to_string())));
    // Created and deleted before the writer ran: no file is left behind.
    store.create("Brief").unwrap();
    store.delete("Brief").unwrap();

    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Keep.m3u8"]);
}

#[test]
fn test_edits_of_a_name_outside_the_playlists_dir_are_refused() {
    common::isolate();
    let dir = scratch("unsafe");
    let outside = dir.parent().unwrap().join("escape.m3u8");
    fs::write(&outside, "keep me\n").unwrap();
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir.clone(), worker.handle());
    // `create` would never accept this name; this covers a bad name that got into memory.
    store.set_loaded(vec![Playlist::new("../escape")]);

    assert_eq!(store.add("../escape", vec![entry("a")]), Err(StoreError::UnsafePath));
    assert_eq!(store.remove("../escape", 0), Err(StoreError::UnsafePath));
    assert_eq!(store.move_entry("../escape", 0, MoveDir::Down), Err(StoreError::UnsafePath));
    assert_eq!(store.rename("../escape", "Fine"), Err(StoreError::UnsafePath));
    assert_eq!(store.delete("../escape"), Err(StoreError::UnsafePath));
    assert_eq!(
        StoreError::UnsafePath.to_string(),
        "Refusing to write outside the playlists folder"
    );
    assert_eq!(store.get("../escape").map(|p| p.entries.len()), Some(0));

    assert!(worker.flush());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep me\n");
    assert!(!dir.exists());
}

#[test]
fn test_non_utf8_file_is_skipped_and_never_rewritten() {
    common::isolate();
    let dir = scratch("non_utf8");
    fs::create_dir_all(&dir).unwrap();
    let latin1: &[u8] = b"#EXTM3U\n/music/caf\xe9.flac\n";
    fs::write(dir.join("Latin.m3u8"), latin1).unwrap();
    fs::write(dir.join("Good.m3u8"), "#EXTM3U\n/music/good.flac\n").unwrap();

    let (worker, mut store) = open_store(&dir);
    assert_eq!(names(&store), vec!["Good"]);
    store.add("Good", vec![entry("a")]).unwrap();
    store.create("Other").unwrap();

    assert!(worker.flush());
    assert_eq!(fs::read(dir.join("Latin.m3u8")).unwrap(), latin1);
    assert_eq!(file_names(&dir), vec!["Good.m3u8", "Latin.m3u8", "Other.m3u8"]);
}

#[test]
fn test_unreadable_file_is_skipped_and_never_rewritten() {
    common::isolate();
    let dir = scratch("unreadable");
    fs::create_dir_all(&dir).unwrap();
    let locked = dir.join("Locked.m3u8");
    let text = "#EXTM3U\n/music/locked.flac\n";
    fs::write(&locked, text).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        // Running as root: permissions don't stop the read, so there's nothing to test.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }

    let (worker, mut store) = open_store(&dir);
    assert!(store.playlists().is_empty());
    assert_eq!(
        store.create("locked"),
        Err(StoreError::Name(NameError::Duplicate("Locked".to_string())))
    );
    store.create("Fresh").unwrap();
    assert!(worker.flush());

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(fs::read_to_string(&locked).unwrap(), text);
    assert_eq!(file_names(&dir), vec!["Fresh.m3u8", "Locked.m3u8"]);
}
```

- [ ] **Step 7: Run the tests and confirm they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlist_store -- --test-threads=1`

Expected: the build fails with ``error[E0599]: no method named `remove` found for struct `PlaylistStore` ``. The same error appears for `move_entry`, `rename` and `delete`.

- [ ] **Step 8: Write remove, move_entry, rename and delete**

In `src/playlist_store.rs`, insert the new methods right after the end of `add`. The existing anchor lines are:

```rust
        let total = playlist.entries.len();
        if added > 0 {
            self.write_through(slot, path);
        }
        Ok(AddResult { added, skipped, total })
    }
```

Insert after that closing `}`:

```rust

    /// Remove and return track `index` of playlist `name`.
    pub fn remove(&mut self, name: &str, index: usize) -> Result<PlaylistEntry, StoreError> {
        let (slot, path) = self.locate(name)?;
        let entries = &mut self.playlists[slot].entries;
        if index >= entries.len() {
            return Err(StoreError::IndexOutOfRange);
        }
        let removed = entries.remove(index);
        self.write_through(slot, path);
        Ok(removed)
    }

    /// Move track `index` one step up or down and return its new index. Moves don't wrap
    /// around: at an edge the index is returned unchanged and nothing is written.
    pub fn move_entry(&mut self, name: &str, index: usize, dir: MoveDir) -> Result<usize, StoreError> {
        let (slot, path) = self.locate(name)?;
        let entries = &mut self.playlists[slot].entries;
        if index >= entries.len() {
            return Err(StoreError::IndexOutOfRange);
        }
        let target = match dir {
            MoveDir::Up if index > 0 => index - 1,
            MoveDir::Down if index + 1 < entries.len() => index + 1,
            _ => return Ok(index),
        };
        entries.swap(index, target);
        self.write_through(slot, path);
        Ok(target)
    }

    /// Rename playlist `old` to `new` and return the stored (trimmed) new name. The duplicate
    /// check ignores the playlist being renamed, so a case-only rename is allowed. A name
    /// reserved by a file skipped at load is refused, and so is a case-only rename onto a
    /// reserved name with exactly the same bytes.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<String, StoreError> {
        let (slot, old_path) = self.locate(old)?;
        let new_name = validate_name(new).map_err(StoreError::Name)?;
        if let Some(other) = self
            .playlists
            .iter()
            .enumerate()
            .find(|(i, p)| *i != slot && names_equal(&p.name, &new_name))
            .map(|(_, p)| p)
        {
            return Err(StoreError::Name(NameError::Duplicate(other.name.clone())));
        }
        // A case-only rename keeps the playlist's own file, so only a different name can land
        // on a skipped file.
        if !names_equal(&self.playlists[slot].name, &new_name) {
            self.ensure_not_reserved(&new_name)?;
        }
        // Except on a case-sensitive disk, where `set_loaded` may have dropped `mix.m3u8` next
        // to the kept `Mix.m3u8`: renaming `Mix` to exactly `mix` would replace that file.
        if let Some(r) = self.reserved.iter().find(|r| **r == new_name) {
            return Err(StoreError::Name(NameError::Duplicate(r.clone())));
        }
        let new_path = self.file_for(&new_name)?;
        if self.playlists[slot].name == new_name {
            return Ok(new_name);
        }
        self.playlists[slot].name = new_name.clone();
        let bytes = self.playlists[slot].to_m3u8().into_bytes();
        sort_by_name(&mut self.playlists);
        // Move with the OS rename, never "write the new file, delete the old one": on a
        // case-insensitive disk `road trip.m3u8` and `Road Trip.m3u8` are one file, so the
        // delete would remove the playlist just written.
        self.persist.rename(old_path, new_path.clone());
        // Save again so the `#PLAYLIST` line carries the new name.
        self.persist.save(new_path, bytes);
        Ok(new_name)
    }

    /// Delete playlist `name` and its file.
    pub fn delete(&mut self, name: &str) -> Result<(), StoreError> {
        let (slot, path) = self.locate(name)?;
        self.playlists.remove(slot);
        self.persist.delete(path);
        Ok(())
    }
```

- [ ] **Step 9: Run the tests and confirm they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlist_store -- --test-threads=1`

Expected: `test result: ok. 23 passed; 0 failed`.

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_isolation -- --test-threads=1`

Expected: `test result: ok.` This confirms that the new test file declares `mod common;` and that every test calls `common::isolate()` first.

- [ ] **Step 10: Format, lint and run the full suite**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

Expected: `cargo fmt --check` prints nothing, clippy finishes with no warnings, and every test binary reports `test result: ok.`

- [ ] **Step 11: Commit**

```bash
git add src/playlist_store.rs src/lib.rs tests/test_playlist_store.rs
git commit -m "feat: PlaylistStore with write-through edits to the playlists folder (#37)

In-memory playlists sorted by name ignoring case. Each edit goes through the background
writer: create, add (skips duplicate paths), remove, move, rename (OS rename, then save
again, so a case-only rename keeps one file) and delete. load_dir_reserving runs on a
worker. It reads only files ending in exactly .m3u8, so every loaded playlist is a file the
store can rewrite, rename and delete on case-sensitive disks too. It skips other extension
cases, non-files, unreadable, non-UTF-8 and hidden files, and returns the skipped stems;
reserve_names makes create and rename refuse those names, so a skipped file is never
overwritten, even on a case-insensitive disk. set_loaded reserves the names it drops as case
duplicates, and rename refuses even a case-only rename onto a byte-equal reserved name, so
mix.m3u8 next to Mix.m3u8 on a case-sensitive disk is never replaced either. Paths outside
the playlists folder are refused.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```


---

### Task 4: Tab 5 shell (navigation and rendering)

**Files:**
- Create: `src/ui/playlists.rs`
- Modify: `src/ui/mod.rs`
  - module list at the top (`pub mod search;` / `pub mod workers;`, lines 13-14): add `pub mod playlists;`
  - `pub struct AppUI`: add a field after `pub persistence: crate::persist::PersistenceWorker,` (line 270)
  - `AppUI::new`: after `settings_mgr.set_persistence(persistence.handle());` (line 292), in the struct literal after `persistence,` (line 402), and after `app.refresh_browser();` (line 404)
  - `event_loop`: after `self.poll_pending_queue_adds();` (line 1596)
  - `handle_input_key`: between the end of the `editing_home_dir` block and `// Global hotkeys` (line 2039); the `Tab` / `BackTab` arms (lines 2074-2079); after the `KeyCode::Char('4')` arm (line 2089)
  - `handle_tab_key_with_modifiers`: the final `_ => {}` of `match self.active_tab`, after the `3 => { // Settings Tab ... }` arm (line 2660)
  - `render`: the `match self.active_tab` (lines 3508-3514) and the modal `if let ... else if` chain (lines 3518-3524)
  - `render_tabs`: the `titles` vec (lines 3528-3533)
  - `render_help_modal`: `let area = help_modal_rect(f.area());` (line 4631), the `"1 - 4"` help line (line 4685), and the end of `left_lines` after the `"~"` line (line 4691)
  - `fn help_modal_rect` (line 4891)
- Test: `tests/test_playlists_ui.rs` (new; Tasks 5-7 add to it)

**Interfaces:**
- Consumes:
  - Task 1: `quickstream::playlist::{Playlist, PlaylistEntry}` (`Playlist::to_m3u8`, `Playlist::total_duration_secs`, `PlaylistEntry::display_label`) and `paths::get_playlists_dir() -> PathBuf`
  - Task 3: `PlaylistStore::{new, load_dir_reserving, set_loaded, reserve_names, is_loaded, dir, playlists, create, delete, remove}`, `StoreError::Name` and `NameError::Duplicate`
    - `load_dir_reserving(dir) -> (Vec<Playlist>, Vec<String>)` returns the loaded playlists and the stems of the files it skipped (unreadable, not UTF-8, or an extension case other than `m3u8`, such as `Mix.M3U8`).
    - `reserve_names(skipped)` makes `create` and `rename` refuse those names with `StoreError::Name(NameError::Duplicate(<stem>))`, so no edit writes over a skipped file (spec Section 1).
  - Existing code: `crate::persist::PersistHandle`, `PersistenceWorker::handle()`, `WorkerRegistry::spawn(&'static str, F)`, the private `super::centered_rect(u16, u16, Rect) -> Rect` (visible to the child module), and `crate::log_info!` / `crate::log_error!`
- Produces, matching the contract:
  ```rust
  // src/ui/mod.rs
  pub mod playlists;
  pub playlists: playlists::PlaylistsUi,               // AppUI field
  // src/ui/playlists.rs
  pub struct PlaylistsUi {
      pub store: PlaylistStore, pub list_cursor: usize, pub track_cursor: usize, pub tracks_focus: bool,
      pub modal: Option<PlaylistModal>, pub exists: HashMap<PathBuf, bool>,
      /* private: load_rx: Option<mpsc::Receiver<(Vec<Playlist>, Vec<String>)>>, load_failed: bool, list_state: ListState, tracks_state: ListState */
  }
  #[derive(Debug, Clone)] pub enum PlaylistModal { NamePrompt { purpose: NamePurpose, input: String, error: Option<String> }, ConfirmDelete { name: String, count: usize }, AddPicker { pending: PendingAdd, cursor: usize } }
  #[derive(Debug, Clone)] pub enum NamePurpose { Create, Rename { old: String }, CreateAndAdd(PendingAdd) }
  #[derive(Debug, Clone)] pub enum PendingAdd { Entries { label: String, entries: Vec<PlaylistEntry> }, Folder { label: String, dir: PathBuf } }
  impl PlaylistsUi { pub fn new(persist: crate::persist::PersistHandle) -> Self; pub fn selected_name(&self) -> Option<String>; }
  impl AppUI {
      pub fn start_playlist_load(&mut self);
      pub fn poll_playlists(&mut self);
      pub fn handle_playlist_modal_key(&mut self, key: crossterm::event::KeyEvent) -> anyhow::Result<bool>;
      pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers);
      pub(super) fn render_playlists_tab(&mut self, f: &mut Frame, area: Rect);
      pub(super) fn render_playlist_modal(&self, f: &mut Frame);
  }
  ```
  The contract does not list a visibility for the last three methods. They are `pub(super)` because a private method defined in `ui::playlists` cannot be called from `ui/mod.rs`.
- Produces, added by this task in its own file for Tasks 5-7:
  ```rust
  pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";             // last row of the add picker
  impl PlaylistsUi {
      pub fn selected_playlist(&self) -> Option<&Playlist>;
      pub fn clamp_cursors(&mut self);                                // called by poll, key handler and render
  }
  pub fn format_duration(secs: i64) -> String;                       // "4:44", "1:02:03", "-:--" when < 0
  pub fn track_count_label(count: usize) -> String;                  // "1 track" / "24 tracks"
  pub fn track_row_label(index: usize, entry: &PlaylistEntry) -> String; // "1. Radiohead - Airbag  4:44"
  ```
- Notes for later tasks:
  - `render_playlist_modal` already draws all three modal variants: the name prompt with its error line, `Delete "<name>" (<n> tracks)? y/n`, and the picker rows plus `NEW_PLAYLIST_ROW` with the `cursor` row highlighted. Tasks 5 and 6 only need to add key handling to `handle_playlist_modal_key`.
  - The `Enter` arm only applies when `!tracks_focus`, so Task 7 can add `Enter` for the tracks pane.
  - `HELP_MODAL_HEIGHT = 30` leaves room for 7 more lines in the left help column.

- [ ] **Step 1: Write the failing tests**

Create `tests/test_playlists_ui.rs`:

```rust
//! Tab 5 (`[5] Playlists`): tab switching, two-pane navigation, rendering and the modal hook.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use quickstream::paths;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::StoreError;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{NamePurpose, PlaylistModal};
use quickstream::ui::AppUI;

/// A fresh temp dir for one test. Also empties the playlists folder, which every test in this
/// binary shares through the one isolated config dir.
fn fresh_dirs(tag: &str) -> PathBuf {
    let _ = fs::remove_dir_all(paths::get_playlists_dir());
    let temp_dir = std::env::temp_dir().join(format!("quickstream_pl_ui_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    temp_dir
}

fn entry(dir: &Path, file: &str, artist: &str, title: &str, secs: i64) -> PlaylistEntry {
    PlaylistEntry {
        path: dir.join(file),
        duration_secs: secs,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Write a playlist file straight into the playlists folder, as if saved by an earlier run.
fn write_playlist(name: &str, entries: Vec<PlaylistEntry>) {
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let playlist = Playlist {
        name: name.to_string(),
        entries,
    };
    fs::write(dir.join(format!("{}.m3u8", name)), playlist.to_m3u8()).unwrap();
}

fn new_app(temp_dir: &Path) -> AppUI {
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    AppUI::new(Some(temp_dir.to_path_buf()), settings_mgr)
}

/// Poll until the startup load has reached the store (bounded, ~2s).
fn wait_loaded(app: &mut AppUI) {
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("playlists did not finish loading");
}

fn press(app: &mut AppUI, code: KeyCode) -> bool {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
}

fn render(app: &mut AppUI, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_key_5_and_tab_cycling_cover_five_tabs() {
    common::isolate();
    let temp_dir = fresh_dirs("tabs");
    let mut app = new_app(&temp_dir);

    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active_tab, 4);

    // Tab wraps from the playlists tab to Now Playing, BackTab wraps back.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.active_tab, 0);
    app.handle_input_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .unwrap();
    assert_eq!(app.active_tab, 4);

    // Settings -> Playlists with Tab.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.active_tab, 4);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_shows_loading_then_empty_state() {
    common::isolate();
    let temp_dir = fresh_dirs("empty");
    let mut app = new_app(&temp_dir);
    app.active_tab = 4;

    // Nothing has been polled yet, so the store is still loading.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("[5] Playlists"), "tab bar should list tab 5:\n{}", view);
    assert!(view.contains("Loading playlists…"), "{}", view);

    wait_loaded(&mut app);
    let view = render(&mut app, 120, 30);
    assert!(!view.contains("Loading playlists…"), "{}", view);
    assert!(view.contains("No playlists yet."), "{}", view);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_startup_load_reserves_the_names_of_skipped_files() {
    common::isolate();
    let temp_dir = fresh_dirs("reserved");
    // Spec Section 1: a file the load skips is never rewritten. `Latin.m3u8` is not UTF-8 and
    // `Mix.M3U8` has an upper-case extension. On a case-insensitive disk `latin.m3u8` and
    // `mix.m3u8` are those same files.
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let latin1: &[u8] = b"#EXTM3U\n/music/caf\xe9.flac\n";
    fs::write(dir.join("Latin.m3u8"), latin1).unwrap();
    let mix = "#EXTM3U\n/music/mix.flac\n";
    fs::write(dir.join("Mix.M3U8"), mix).unwrap();

    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    assert!(app.playlists.store.playlists().is_empty());

    assert_eq!(
        app.playlists.store.create("latin"),
        Err(StoreError::Name(NameError::Duplicate("Latin".to_string())))
    );
    assert_eq!(
        app.playlists.store.create("MIX"),
        Err(StoreError::Name(NameError::Duplicate("Mix".to_string())))
    );
    assert!(app.playlists.store.playlists().is_empty());

    assert!(app.persistence.flush());
    assert_eq!(fs::read(dir.join("Latin.m3u8")).unwrap(), latin1);
    assert_eq!(fs::read_to_string(dir.join("Mix.M3U8")).unwrap(), mix);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_renders_rows_titles_and_empty_playlist() {
    common::isolate();
    let temp_dir = fresh_dirs("render");
    write_playlist(
        "Road Trip",
        vec![
            entry(&temp_dir, "01 Airbag.flac", "Radiohead", "Airbag", 284),
            entry(&temp_dir, "02 Paranoid Android.flac", "", "Paranoid Android", -1),
        ],
    );
    write_playlist("chill", Vec::new());
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    // Sorted case-insensitively: "chill" comes first and is selected.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("chill  (0 tracks)"), "{}", view);
    assert!(view.contains("Road Trip  (2 tracks)"), "{}", view);
    assert!(
        view.contains("Empty. Press L on a track in tab 2 to add it."),
        "{}",
        view
    );

    // Down selects Road Trip; the unknown duration is left out of the total.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Road Trip"));
    let view = render(&mut app, 120, 30);
    assert!(view.contains("ROAD TRIP (2 tracks, 4:44)"), "{}", view);
    assert!(view.contains("1. Radiohead - Airbag  4:44"), "{}", view);
    assert!(view.contains("2. Paranoid Android  -:--"), "{}", view);

    // A small terminal must not panic.
    render(&mut app, 40, 12);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_tab_focus_and_wraparound_navigation() {
    common::isolate();
    let temp_dir = fresh_dirs("nav");
    write_playlist("Alpha", vec![entry(&temp_dir, "a1.flac", "A", "One", 60)]);
    write_playlist(
        "Bravo",
        vec![
            entry(&temp_dir, "b1.flac", "B", "One", 60),
            entry(&temp_dir, "b2.flac", "B", "Two", 60),
            entry(&temp_dir, "b3.flac", "B", "Three", 60),
        ],
    );
    write_playlist("Charlie", Vec::new());
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.playlists.list_cursor, 0);
    assert!(!app.playlists.tracks_focus);

    // Left pane wraps both ways (k / Up at the top, j / Down at the bottom).
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.list_cursor, 2);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.list_cursor, 0);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.playlists.list_cursor, 2);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Bravo"));

    // Enter on the left pane focuses the tracks pane; h / l switch back and forth.
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.tracks_focus);
    press(&mut app, KeyCode::Char('h'));
    assert!(!app.playlists.tracks_focus);
    press(&mut app, KeyCode::Char('l'));
    assert!(app.playlists.tracks_focus);

    // Tracks pane wraps both ways and leaves the playlist selection alone.
    assert_eq!(app.playlists.track_cursor, 0);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.track_cursor, 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.playlists.track_cursor, 0);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.track_cursor, 1);
    assert_eq!(app.playlists.list_cursor, 1);

    // Moving to another playlist starts its tracks at the top.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Charlie"));
    assert_eq!(app.playlists.track_cursor, 0);

    // The keys only act on tab 5: on tab 4 (Settings) 'j' moves the settings cursor instead.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.playlists.list_cursor, 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlists_cursors_clamp_when_lists_shrink() {
    common::isolate();
    let temp_dir = fresh_dirs("clamp");
    write_playlist("a", vec![entry(&temp_dir, "a1.flac", "A", "One", 60)]);
    write_playlist(
        "b",
        vec![
            entry(&temp_dir, "b1.flac", "B", "One", 60),
            entry(&temp_dir, "b2.flac", "B", "Two", 60),
            entry(&temp_dir, "b3.flac", "B", "Three", 60),
        ],
    );
    write_playlist("c", vec![entry(&temp_dir, "c1.flac", "C", "One", 60)]);
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    // Select the last playlist, then remove it from the store.
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.list_cursor, 2);
    app.playlists.store.delete("c").unwrap();
    app.poll_playlists();
    assert_eq!(app.playlists.list_cursor, 1);
    assert_eq!(app.playlists.selected_name().as_deref(), Some("b"));

    // Select the last track of "b", then remove it.
    press(&mut app, KeyCode::Char('l'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.playlists.track_cursor, 2);
    app.playlists.store.remove("b", 2).unwrap();
    let _ = render(&mut app, 120, 30);
    assert_eq!(app.playlists.track_cursor, 1);

    // Everything gone: both cursors back at 0 and the empty state renders.
    app.playlists.store.delete("a").unwrap();
    app.playlists.store.delete("b").unwrap();
    app.poll_playlists();
    assert_eq!(app.playlists.list_cursor, 0);
    assert_eq!(app.playlists.track_cursor, 0);
    assert_eq!(app.playlists.selected_name(), None);
    let view = render(&mut app, 120, 30);
    assert!(view.contains("No playlists yet."), "{}", view);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_playlist_modal_takes_keys_before_global_hotkeys() {
    common::isolate();
    let temp_dir = fresh_dirs("modal");
    let mut app = new_app(&temp_dir);
    wait_loaded(&mut app);
    press(&mut app, KeyCode::Char('5'));

    app.playlists.modal = Some(PlaylistModal::NamePrompt {
        purpose: NamePurpose::Create,
        input: String::new(),
        error: None,
    });
    let view = render(&mut app, 120, 30);
    assert!(view.contains("NEW PLAYLIST"), "{}", view);

    // Neither a tab key nor q (quit) reaches the global hotkeys while the modal is open.
    assert!(!press(&mut app, KeyCode::Char('1')));
    assert_eq!(app.active_tab, 4);
    assert!(!press(&mut app, KeyCode::Char('q')));
    assert!(app.playlists.modal.is_some());

    // Esc closes it; afterwards keys reach the tab again.
    assert!(!press(&mut app, KeyCode::Esc));
    assert!(app.playlists.modal.is_none());
    press(&mut app, KeyCode::Char('1'));
    assert_eq!(app.active_tab, 0);

    // Ctrl+C quits from inside the modal, like the other modals.
    app.playlists.modal = Some(PlaylistModal::ConfirmDelete {
        name: "Road Trip".to_string(),
        count: 24,
    });
    let view = render(&mut app, 120, 30);
    assert!(view.contains("Delete \"Road Trip\" (24 tracks)? y/n"), "{}", view);
    let quit = app
        .handle_input_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(quit);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lists_tab_5_navigation() {
    common::isolate();
    let temp_dir = fresh_dirs("help");
    let mut app = new_app(&temp_dir);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help_modal);

    let view = render(&mut app, 100, 40);
    assert!(view.contains("1 - 5"), "{}", view);
    assert!(view.contains("PLAYLISTS (TAB 5)"), "{}", view);
    assert!(view.contains("Focus Playlists"), "{}", view);
    assert!(view.contains("Open playlist's tracks"), "{}", view);

    // The 20-row terminal the existing help test uses still renders without panicking.
    render(&mut app, 70, 20);

    let _ = fs::remove_dir_all(&temp_dir);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_ui -- --test-threads=1`

Expected: the test file does not compile. The errors include `error[E0432]: unresolved import `quickstream::ui::playlists``, `no field `playlists` on type `AppUI`` and `no method named `poll_playlists` found for struct `AppUI``.

- [ ] **Step 3: Create `src/ui/playlists.rs`**

```rust
//! Tab 5 (`[5] Playlists`): hand-curated playlists on the left, the tracks of the selected one on
//! the right (issue #37). The store is read by a worker at startup and every edit is written
//! through to disk by the persistence worker, so nothing here touches the disk on the UI thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::AppUI;
use crate::paths;
use crate::playlist::{Playlist, PlaylistEntry};
use crate::playlist_store::PlaylistStore;

/// Last row of the add picker: create a playlist and add to it.
pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";

/// What the name prompt's name is for.
#[derive(Debug, Clone)]
pub enum NamePurpose {
    Create,
    Rename {
        old: String,
    },
    /// "+ New playlist…" in the add picker: create the playlist, then add `PendingAdd` to it.
    CreateAndAdd(PendingAdd),
}

/// Tracks waiting for the user to choose a playlist.
#[derive(Debug, Clone)]
pub enum PendingAdd {
    /// Entries built from metadata the UI already holds; `label` is shown in the picker header.
    Entries { label: String, entries: Vec<PlaylistEntry> },
    /// A browser folder; a worker lists its audio files once a playlist is chosen.
    Folder { label: String, dir: PathBuf },
}

/// Modal owned by the playlists feature. While one is open it takes every key.
#[derive(Debug, Clone)]
pub enum PlaylistModal {
    NamePrompt {
        purpose: NamePurpose,
        input: String,
        error: Option<String>,
    },
    ConfirmDelete {
        name: String,
        count: usize,
    },
    AddPicker {
        pending: PendingAdd,
        cursor: usize,
    },
}

/// Tab 5 state, held by `AppUI::playlists`.
pub struct PlaylistsUi {
    pub store: PlaylistStore,
    /// Selected row in the playlists (left) pane.
    pub list_cursor: usize,
    /// Selected row in the tracks (right) pane.
    pub track_cursor: usize,
    /// false = playlists pane focused, true = tracks pane.
    pub tracks_focus: bool,
    pub modal: Option<PlaylistModal>,
    /// `Path::exists()` results for playlist entries, cached for the session.
    pub exists: HashMap<PathBuf, bool>,
    /// Result of the startup load (playlists, stems of skipped files); `None` once applied.
    load_rx: Option<mpsc::Receiver<(Vec<Playlist>, Vec<String>)>>,
    /// The load worker died without sending; the store stays unloaded so no edit can overwrite
    /// a file that was never read.
    load_failed: bool,
    list_state: ListState,
    tracks_state: ListState,
}

impl PlaylistsUi {
    pub fn new(persist: crate::persist::PersistHandle) -> Self {
        Self {
            store: PlaylistStore::new(paths::get_playlists_dir(), persist),
            list_cursor: 0,
            track_cursor: 0,
            tracks_focus: false,
            modal: None,
            exists: HashMap::new(),
            load_rx: None,
            load_failed: false,
            list_state: ListState::default(),
            tracks_state: ListState::default(),
        }
    }

    /// Playlist under the left-pane cursor.
    pub fn selected_playlist(&self) -> Option<&Playlist> {
        self.store.playlists().get(self.list_cursor)
    }

    /// Name of the playlist under the left-pane cursor.
    pub fn selected_name(&self) -> Option<String> {
        self.selected_playlist().map(|p| p.name.clone())
    }

    /// Keep both cursors inside their lists after playlists or tracks went away.
    pub fn clamp_cursors(&mut self) {
        let playlists = self.store.playlists().len();
        if self.list_cursor >= playlists {
            self.list_cursor = playlists.saturating_sub(1);
        }
        let tracks = self.selected_playlist().map_or(0, |p| p.entries.len());
        if self.track_cursor >= tracks {
            self.track_cursor = tracks.saturating_sub(1);
        }
        if playlists == 0 {
            self.tracks_focus = false;
        }
    }

    /// Move the cursor of the focused pane one row, wrapping at both ends.
    fn move_cursor(&mut self, down: bool) {
        if self.tracks_focus {
            let tracks = self.selected_playlist().map_or(0, |p| p.entries.len());
            self.track_cursor = wrap_step(self.track_cursor, tracks, down);
        } else {
            let before = self.list_cursor;
            self.list_cursor = wrap_step(self.list_cursor, self.store.playlists().len(), down);
            if self.list_cursor != before {
                self.track_cursor = 0;
            }
        }
    }
}

/// Next row in a list of `len` rows, wrapping from the last row to the first and back.
fn wrap_step(cursor: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        0
    } else if down {
        (cursor + 1) % len
    } else if cursor == 0 {
        len - 1
    } else {
        cursor - 1
    }
}

/// `m:ss`, or `h:mm:ss` from one hour; `-:--` when the length is unknown (negative).
pub fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "-:--".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// "1 track" / "24 tracks".
pub fn track_count_label(count: usize) -> String {
    if count == 1 {
        "1 track".to_string()
    } else {
        format!("{} tracks", count)
    }
}

/// Tracks-pane row for the entry at `index`: `<n>. <artist> - <title>  <m:ss>`.
pub fn track_row_label(index: usize, entry: &PlaylistEntry) -> String {
    format!(
        "{}. {}  {}",
        index + 1,
        entry.display_label(),
        format_duration(entry.duration_secs)
    )
}

impl AppUI {
    /// Read the playlists folder on a worker; `poll_playlists` hands the result to the store.
    /// The worker also returns the names of the files it skipped, so the store can keep them
    /// reserved and never write over them.
    pub fn start_playlist_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.playlists.load_rx = Some(rx);
        self.playlists.load_failed = false;
        let dir = self.playlists.store.dir().to_path_buf();
        self.workers.spawn("playlist-load", move || {
            let _ = tx.send(PlaylistStore::load_dir_reserving(&dir));
        });
    }

    /// Apply finished playlist work without blocking. Called every UI tick.
    pub fn poll_playlists(&mut self) {
        if let Some(rx) = self.playlists.load_rx.as_ref() {
            match rx.try_recv() {
                Ok((playlists, skipped)) => {
                    crate::log_info!(
                        "playlists",
                        "Loaded {} playlists ({} skipped files keep their names reserved)",
                        playlists.len(),
                        skipped.len()
                    );
                    self.playlists.store.set_loaded(playlists);
                    self.playlists.store.reserve_names(skipped);
                    self.playlists.load_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    crate::log_error!("playlists", "Playlist load worker exited without a result");
                    self.playlists.load_rx = None;
                    self.playlists.load_failed = true;
                    self.status_message = "Couldn't load playlists".to_string();
                }
            }
        }
        self.playlists.clamp_cursors();
    }

    /// Keys while a playlist modal is open. Returns true when the app should quit.
    pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
        }
        if key.code == KeyCode::Esc {
            self.playlists.modal = None;
        }
        Ok(false)
    }

    /// Tab 5 keys that the global hotkeys did not take.
    pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.playlists.clamp_cursors();
        match (code, modifiers) {
            (KeyCode::Char('h'), KeyModifiers::NONE) => self.playlists.tracks_focus = false,
            (KeyCode::Char('l'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE)
                if !self.playlists.tracks_focus && self.playlists.selected_playlist().is_some() =>
            {
                self.playlists.tracks_focus = true;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.playlists.move_cursor(false);
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.playlists.move_cursor(true);
            }
            _ => {}
        }
    }

    pub(super) fn render_playlists_tab(&mut self, f: &mut Frame, area: Rect) {
        self.playlists.clamp_cursors();
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        let focused = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let unfocused = Style::default().fg(Color::DarkGray);
        let selected = Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(Color::DarkGray);
        let empty_tracks = Block::default()
            .borders(Borders::ALL)
            .title(" TRACKS ")
            .border_style(unfocused);

        let ui = &mut self.playlists;
        if !ui.store.is_loaded() {
            let text = if ui.load_failed {
                "Couldn't load playlists. Restart Quickstream to try again."
            } else {
                "Loading playlists…"
            };
            let loading = Paragraph::new(text)
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" PLAYLISTS ")
                        .border_style(focused),
                );
            f.render_widget(loading, split[0]);
            f.render_widget(empty_tracks, split[1]);
            return;
        }

        // Left pane: playlists with their track counts.
        let list_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" PLAYLISTS ({}) ", ui.store.playlists().len()))
            .border_style(if ui.tracks_focus { unfocused } else { focused });
        if ui.store.playlists().is_empty() {
            let empty = Paragraph::new("No playlists yet. Press c to create one, or L on any track.")
                .style(hint)
                .wrap(Wrap { trim: true })
                .block(list_block);
            f.render_widget(empty, split[0]);
            f.render_widget(empty_tracks, split[1]);
            return;
        }
        let rows: Vec<ListItem> = ui
            .store
            .playlists()
            .iter()
            .enumerate()
            .map(|(idx, playlist)| {
                let text = format!("{}  ({})", playlist.name, track_count_label(playlist.entries.len()));
                let style = if idx != ui.list_cursor {
                    Style::default().fg(Color::White)
                } else if ui.tracks_focus {
                    // The open playlist stays marked while its tracks have focus.
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    selected
                };
                ListItem::new(text).style(style)
            })
            .collect();
        ui.list_state.select(Some(ui.list_cursor));
        f.render_stateful_widget(List::new(rows).block(list_block), split[0], &mut ui.list_state);

        // Right pane: the selected playlist's tracks.
        let playlist = &ui.store.playlists()[ui.list_cursor];
        let title = format!(
            " {} ({}, {}) ",
            playlist.name.to_uppercase(),
            track_count_label(playlist.entries.len()),
            format_duration(playlist.total_duration_secs())
        );
        let tracks_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if ui.tracks_focus { focused } else { unfocused });
        if playlist.entries.is_empty() {
            let empty = Paragraph::new("Empty. Press L on a track in tab 2 to add it.")
                .style(hint)
                .wrap(Wrap { trim: true })
                .block(tracks_block);
            f.render_widget(empty, split[1]);
            return;
        }
        let rows: Vec<ListItem> = playlist
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let style = if idx == ui.track_cursor && ui.tracks_focus {
                    selected
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(track_row_label(idx, entry)).style(style)
            })
            .collect();
        ui.tracks_state.select(Some(ui.track_cursor));
        f.render_stateful_widget(List::new(rows).block(tracks_block), split[1], &mut ui.tracks_state);
    }

    pub(super) fn render_playlist_modal(&self, f: &mut Frame) {
        let Some(modal) = self.playlists.modal.as_ref() else {
            return;
        };
        let area = super::centered_rect(60, 50, f.area());
        f.render_widget(Clear, area);

        let text_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(Color::DarkGray);
        let mut scroll = 0u16;
        let (title, lines) = match modal {
            PlaylistModal::NamePrompt { purpose, input, error } => {
                let title = match purpose {
                    NamePurpose::Create | NamePurpose::CreateAndAdd(_) => " NEW PLAYLIST ".to_string(),
                    NamePurpose::Rename { old } => format!(" RENAME \"{}\" ", old),
                };
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Name: ", hint),
                        Span::styled(input.clone(), text_style),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ]),
                    Line::from(""),
                ];
                if let Some(error) = error {
                    lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled("[Enter] Save   [Esc] Cancel", hint)));
                (title, lines)
            }
            PlaylistModal::ConfirmDelete { name, count } => (
                " DELETE PLAYLIST ".to_string(),
                vec![
                    Line::from(Span::styled(
                        format!("Delete \"{}\" ({})? y/n", name, track_count_label(*count)),
                        text_style,
                    )),
                    Line::from(""),
                    Line::from(Span::styled("[y] Delete   any other key: Cancel", hint)),
                ],
            ),
            PlaylistModal::AddPicker { pending, cursor } => {
                let label = match pending {
                    PendingAdd::Entries { label, .. } | PendingAdd::Folder { label, .. } => label,
                };
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!("Add {} to:", label),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                let names = self
                    .playlists
                    .store
                    .playlists()
                    .iter()
                    .map(|p| p.name.as_str())
                    .chain(std::iter::once(NEW_PLAYLIST_ROW));
                for (idx, name) in names.enumerate() {
                    let (prefix, style) = if idx == *cursor {
                        (
                            " ▶ ",
                            Style::default()
                                .bg(Color::Blue)
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("   ", Style::default().fg(Color::White))
                    };
                    lines.push(Line::from(Span::styled(format!("{}{}", prefix, name), style)));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("[Enter] Add   [Esc] Cancel", hint)));
                // Keep the cursor row (after the 2 header lines) inside the visible rows.
                let visible = area.height.saturating_sub(2) as usize;
                scroll = (2 + *cursor + 1).saturating_sub(visible) as u16;
                (" ADD TO PLAYLIST ".to_string(), lines)
            }
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                title,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        let body = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(body, area);
    }
}
```

- [ ] **Step 4: Wire tab 5 into `src/ui/mod.rs`**

Make each edit below with targeted edits. The existing anchor lines are quoted exactly.

4a. Module list (lines 13-14):
```rust
pub mod search;
pub mod workers;
```
becomes
```rust
pub mod playlists;
pub mod search;
pub mod workers;
```

4b. `AppUI` struct, last field:
```rust
    /// Writes bookmarks and settings off the UI thread; flushed and joined on drop.
    pub persistence: crate::persist::PersistenceWorker,
}
```
becomes
```rust
    /// Writes bookmarks and settings off the UI thread; flushed and joined on drop.
    pub persistence: crate::persist::PersistenceWorker,
    /// Tab 5: playlist store, cursors and the playlist modals.
    pub playlists: playlists::PlaylistsUi,
}
```

4c. `AppUI::new`, at the start:
```rust
        settings_mgr.set_persistence(persistence.handle());
        let cwd = start_dir
```
becomes
```rust
        settings_mgr.set_persistence(persistence.handle());
        let playlists = playlists::PlaylistsUi::new(persistence.handle());
        let cwd = start_dir
```
and at the end of the struct literal:
```rust
            mix_picker_cursor: 0,
            persistence,
        };
        app.refresh_browser();
```
becomes
```rust
            mix_picker_cursor: 0,
            persistence,
            playlists,
        };
        app.refresh_browser();
        app.start_playlist_load();
```

4d. `event_loop`:
```rust
            self.poll_pending_queue_adds();

            // Poll progressive background autopilot discovery
```
becomes
```rust
            self.poll_pending_queue_adds();
            self.poll_playlists();

            // Poll progressive background autopilot discovery
```

4e. `handle_input_key`: the `editing_home_dir` block ends with `return Ok(false);` / `}`. Insert the modal hook right before `// Global hotkeys`:
```rust
            return Ok(false);
        }

        // Global hotkeys
        match (key.code, key.modifiers) {
```
becomes
```rust
            return Ok(false);
        }

        // A playlist modal (name prompt, delete confirmation, add picker) takes every key.
        if self.playlists.modal.is_some() {
            return self.handle_playlist_modal_key(key);
        }

        // Global hotkeys
        match (key.code, key.modifiers) {
```

4f. Tab cycling in the global match:
```rust
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.active_tab = (self.active_tab + 1) % 4;
            }
            (KeyCode::BackTab, _) => {
                self.active_tab = (self.active_tab + 3) % 4;
            }
```
becomes
```rust
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.active_tab = (self.active_tab + 1) % 5;
            }
            (KeyCode::BackTab, _) => {
                self.active_tab = (self.active_tab + 4) % 5;
            }
```

4g. After the `'4'` arm:
```rust
            (KeyCode::Char('4'), KeyModifiers::NONE) => {
                self.active_tab = 3;
            }
```
becomes
```rust
            (KeyCode::Char('4'), KeyModifiers::NONE) => {
                self.active_tab = 3;
            }
            (KeyCode::Char('5'), KeyModifiers::NONE) => {
                self.active_tab = 4;
            }
```

4h. `handle_tab_key_with_modifiers`, at the end of the Settings arm:
```rust
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn cycle_and_apply_setting(&mut self, forward: bool) {
```
becomes
```rust
                    _ => {}
                }
            }
            4 => self.handle_playlists_tab_key(code, modifiers),
            _ => {}
        }
    }

    pub fn cycle_and_apply_setting(&mut self, forward: bool) {
```

4i. `render`:
```rust
            3 => self.render_settings_tab(f, chunks[1]),
            _ => {}
```
becomes
```rust
            3 => self.render_settings_tab(f, chunks[1]),
            4 => self.render_playlists_tab(f, chunks[1]),
            _ => {}
```
and
```rust
            Self::render_search_modal(modal, f);
        } else if self.show_mix_picker_modal {
```
becomes
```rust
            Self::render_search_modal(modal, f);
        } else if self.playlists.modal.is_some() {
            self.render_playlist_modal(f);
        } else if self.show_mix_picker_modal {
```

4j. `render_tabs`:
```rust
            " [4] Settings ",
        ];
```
becomes
```rust
            " [4] Settings ",
            " [5] Playlists ",
        ];
```

4k. `render_help_modal`. Use a taller rect so the new section fits:
```rust
    fn render_help_modal(&self, f: &mut Frame) {
        let area = help_modal_rect(f.area());
```
becomes
```rust
    fn render_help_modal(&self, f: &mut Frame) {
        let area = modal_rect_with_height(f.area(), HELP_MODAL_HEIGHT);
```
Then:
```rust
            Self::help_key_line("1 - 4", "Switch tab (Now/Queue/Dev/Set)"),
```
becomes
```rust
            Self::help_key_line("1 - 5", "Switch tab (Now/Queue/Dev/Set/PL)"),
```
and the end of `left_lines`:
```rust
            Self::help_key_line("~", "Jump to startup home dir"),
        ];
```
becomes
```rust
            Self::help_key_line("~", "Jump to startup home dir"),
            Line::from(""),
            Self::help_section_header("PLAYLISTS (TAB 5)"),
            Self::help_key_line("h / l", "Focus Playlists ↔ Tracks"),
            Self::help_key_line("Enter", "Open playlist's tracks"),
        ];
```

4l. `help_modal_rect`. The mix picker keeps 20 rows; the help modal gets its own height:
```rust
fn help_modal_rect(r: Rect) -> Rect {
    let max_w = if r.width >= 96 { 90 } else { r.width.saturating_sub(2) };
    let target_width = max_w.min(r.width);
    let target_height = 20.min(r.height.saturating_sub(2)).min(r.height);
```
becomes
```rust
fn help_modal_rect(r: Rect) -> Rect {
    modal_rect_with_height(r, 20)
}

/// Rows of the help modal: the key list, its footer and the border must fit.
const HELP_MODAL_HEIGHT: u16 = 30;

/// Centered modal up to 90 columns wide and `height` rows tall, shrunk to fit small terminals.
fn modal_rect_with_height(r: Rect, height: u16) -> Rect {
    let max_w = if r.width >= 96 { 90 } else { r.width.saturating_sub(2) };
    let target_width = max_w.min(r.width);
    let target_height = height.min(r.height.saturating_sub(2)).min(r.height);
```
The rest of the function, from `let x = ...` to `Rect::new(...)`, stays as it is.

- [ ] **Step 5: Run the new tests and the UI tests they touch**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_ui --test test_now_playing --test test_browser --test test_isolation -- --test-threads=1`

Expected: all PASS. `test_playlists_ui` reports `8 passed`. The existing `test_now_playing_tab_key_navigation` still passes (`Tab` from tab 1 goes to 2), and `test_help_modal_lifecycle_and_render` still renders at 70x20.

- [ ] **Step 6: Format, lint and run the full suite**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```
Expected: no fmt diff, no clippy warnings, and every test result `ok`. If clippy reports `collapsible_match` on a tab-key arm, keep the guard style used above (`... if !self.playlists.tracks_focus && ... =>`).

- [ ] **Step 7: Commit**

```bash
git add src/ui/playlists.rs src/ui/mod.rs tests/test_playlists_ui.rs
git commit -m "$(cat <<'EOF'
feat: tab 5 Playlists shell with two-pane navigation

Adds the [5] Playlists tab: startup load of the playlists folder on a
worker ("Loading playlists…" until it arrives; files it skips keep
their names reserved so no edit writes over them), playlists and tracks
panes with h/l focus, wrapping j/k/arrow navigation, Enter to open a
playlist's tracks, cursor clamping when lists shrink, empty states, a
modal hook that takes keys before the global hotkeys (Esc closes,
Ctrl+C quits), and help-modal lines for tab 5. Tab/BackTab now cycle
through 5 tabs.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```


---

### Task 5: Tab 5 editing (create, rename, delete, move, remove)

**Files:**
- Modify: `src/ui/playlists.rs` (created by Task 4)
  - `pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {`: keep this signature exactly as Task 4 wrote it. Change only the body: an editing-key dispatch becomes its first statement, before Task 4's `self.playlists.clamp_cursors();` and navigation match.
  - `pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool>`: keep the signature and replace the whole body. Task 4's version only quits on Ctrl+C and closes the modal on Esc.
  - `pub(super) fn render_playlist_modal(&self, f: &mut Frame)`: keep the `pub(super)` signature (`render()` in `src/ui/mod.rs` calls it) and replace the whole body.
  - End of file: add a new `impl AppUI { ... }` block and three free functions.
- Modify: `src/ui/mod.rs`
  - `fn render_help_modal(&self, f: &mut Frame)`: in `left_lines`, add three lines directly after Task 4's `Self::help_key_line("Enter", "Open playlist's tracks"),`, which is the last line of the `PLAYLISTS (TAB 5)` section. `help_modal_rect` and `HELP_MODAL_HEIGHT` are not touched. After Task 4 the left column has 20 lines. This task adds 3 and Task 7 adds 1, for 24 in total. That fits in the 27 rows that Task 4's `HELP_MODAL_HEIGHT = 30` leaves inside the modal. If more rows are ever needed, raise `HELP_MODAL_HEIGHT`, not the literal `20` in `help_modal_rect`. Since Task 4, that literal only sizes the mix picker.
- Create (test): `tests/test_playlists_edit.rs`

**Interfaces:**

Consumes:
- Task 1 (`src/playlist.rs`):
  - `PlaylistEntry::from_path(&Path) -> PlaylistEntry`
  - `PlaylistEntry::display_label(&self) -> String`
  - `parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist`
  - `NameError` messages through `Display`, for example `"Name can't start with ."` and `"\"{0}\" already exists"`
- Task 3 (`src/playlist_store.rs`):
  - `PlaylistStore::new(PathBuf, PersistHandle)`
  - `is_loaded()`, `playlists()`, `get(&str)`, `index_of(&str)`
  - `create(&str) -> Result<String, StoreError>`
  - `rename(&str, &str) -> Result<String, StoreError>`
  - `delete(&str) -> Result<(), StoreError>`
  - `add(&str, Vec<PlaylistEntry>) -> Result<AddResult, StoreError>` (tests only)
  - `remove(&str, usize) -> Result<PlaylistEntry, StoreError>`
  - `move_entry(&str, usize, MoveDir) -> Result<usize, StoreError>`
  - `StoreError::NotLoaded`, which displays as "Playlists are still loading"
  - `MoveDir::{Up, Down}`
- Task 4 (`src/ui/playlists.rs`):
  - `use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};`
  - `PlaylistsUi { store, list_cursor, track_cursor, tracks_focus, modal, exists }`
  - `PlaylistModal::{NamePrompt { purpose, input, error }, ConfirmDelete { name, count }, AddPicker { pending, cursor }}`
  - `NamePurpose::{Create, Rename { old }, CreateAndAdd(PendingAdd)}`
  - `PlaylistsUi::selected_name(&self) -> Option<String>`
  - `pub fn track_count_label(count: usize) -> String` ("1 track" / "24 tracks")
  - `AppUI::poll_playlists(&mut self)`
  - `AppUI::new` calls `start_playlist_load()`.
  - `handle_input_key` calls `handle_playlist_modal_key` when `self.playlists.modal.is_some()`, before the global hotkeys.
  - `render()` calls `self.render_playlist_modal(f)` when a modal is open.
  - `handle_tab_key_with_modifiers` calls `self.handle_playlists_tab_key(code, modifiers)` when `active_tab == 4`.
  - The help modal is sized by `modal_rect_with_height(f.area(), HELP_MODAL_HEIGHT)` with `HELP_MODAL_HEIGHT = 30`.
- Existing code: `AppUI.persistence: PersistenceWorker` (`flush()`, `handle()`), `AppUI.status_message`, `AppUI.player.stop()`, `AppUI::help_key_line`, `paths::get_playlists_dir()`.

Produces. Everything is private to `ui::playlists` unless it is marked `pub` or `pub(super)`:
```rust
pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers); // Task 4's signature; body now starts with the edit dispatch
pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool>; // handles NamePrompt + ConfirmDelete; AddPicker arm stays Esc-only for Task 6
pub(super) fn render_playlist_modal(&self, f: &mut Frame); // draws NamePrompt + ConfirmDelete; AddPicker arm empty for Task 6
fn handle_playlist_edit_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool;
fn open_create_prompt(&mut self);
fn open_rename_prompt(&mut self);
fn open_delete_confirm(&mut self);
fn submit_name_prompt(&mut self, purpose: NamePurpose, input: String); // Task 6 splits its CreateAndAdd status arm (see notes)
fn delete_playlist(&mut self, name: &str);
fn move_selected_track(&mut self, dir: crate::playlist_store::MoveDir);
fn remove_selected_track(&mut self);
// free fns in src/ui/playlists.rs
fn playlist_prompt_rect(r: Rect, height: u16) -> Rect;
fn render_name_prompt(f: &mut Frame, purpose: &NamePurpose, input: &str, error: Option<&str>);
fn render_confirm_delete(f: &mut Frame, name: &str, count: usize);
```

Notes for Task 6:
- Task 6 handles AddPicker keys, and Enter in a `CreateAndAdd` name prompt, before this task's handler runs (`handle_add_picker_key`, `confirm_create_and_add`). Leave this task's `CreateAndAdd` and `AddPicker` arms as they are; they are not reached once Task 6 is applied.

Status messages added by this task:
- `Created "<name>"`
- `Renamed "<old>" to "<new>"`
- `Deleted "<name>"`
- `Kept "<name>"` (delete cancelled)
- `Cancelled` (prompt closed with Esc)
- `Removed "<label>" from <name>`
- `Playlists are still loading`

---

- [ ] **Step 1: Write the failing tests for the name prompt and delete confirmation**

Create `tests/test_playlists_edit.rs`:

```rust
//! Tab 5 editing: name prompt (create / rename), delete confirmation, moving and removing tracks.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::paths;
use quickstream::playlist::PlaylistEntry;
use quickstream::playlist_store::PlaylistStore;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{NamePurpose, PlaylistModal};
use quickstream::ui::AppUI;

/// A fresh app on tab 5 with a loaded, empty playlists store. Tests in this binary share one
/// isolated config dir, so the playlists folder is removed first.
fn app_on_playlists_tab(tag: &str) -> (AppUI, PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_pl_edit_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let _ = fs::remove_dir_all(paths::get_playlists_dir());

    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.playlists.store.is_loaded(), "playlists did not load within 2s");
    app.active_tab = 4;
    (app, temp_dir)
}

/// Send one key; returns true when the app asked to quit.
fn press(app: &mut AppUI, code: KeyCode, modifiers: KeyModifiers) -> bool {
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap()
}

/// Type `text`, sending upper-case letters with Shift as terminals do.
fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        let modifiers = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        press(app, KeyCode::Char(c), modifiers);
    }
}

/// Create playlist `name` holding `tracks` directly through the store.
fn seed(app: &mut AppUI, name: &str, tracks: &[PathBuf]) {
    let stored = app.playlists.store.create(name).unwrap();
    let entries = tracks.iter().map(|p| PlaylistEntry::from_path(p.as_path())).collect();
    app.playlists.store.add(&stored, entries).unwrap();
}

fn playlist_names(app: &AppUI) -> Vec<String> {
    app.playlists.store.playlists().iter().map(|p| p.name.clone()).collect()
}

/// File names in the playlists folder, sorted.
fn playlist_files() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(paths::get_playlists_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn render_view(app: &mut AppUI, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_create_prompt_types_q_and_digits_and_selects_new_playlist() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("create");
    seed(&mut app, "Alpha", &[]);
    seed(&mut app, "Zulu", &[]);
    app.playlists.list_cursor = 0;

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(
        matches!(
            app.playlists.modal,
            Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::Create,
                ..
            })
        ),
        "c should open the name prompt"
    );

    // In the prompt, q, digits and M are typed as text. They do not quit, switch tabs or open the mix picker.
    type_text(&mut app, "Mq 2");
    assert_eq!(app.active_tab, 4);
    assert!(!app.show_mix_picker_modal);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { input, error, .. }) => {
            assert_eq!(input, "Mq 2");
            assert!(error.is_none());
        }
        _ => panic!("the prompt closed while typing"),
    }

    assert!(!press(&mut app, KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.playlists.modal.is_none(), "Enter with a valid name closes the prompt");
    assert_eq!(playlist_names(&app), vec!["Alpha", "Mq 2", "Zulu"]);
    assert_eq!(
        app.playlists.list_cursor, 1,
        "the cursor follows the new playlist to its sorted row"
    );
    assert_eq!(app.status_message, "Created \"Mq 2\"");

    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Alpha.m3u8", "Mq 2.m3u8", "Zulu.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_name_prompt_shows_errors_and_stays_open() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("errors");
    seed(&mut app, "Road Trip", &[]);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(
        matches!(app.playlists.modal, Some(PlaylistModal::NamePrompt { .. })),
        "c should open the name prompt"
    );
    type_text(&mut app, "road trip");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            input, error: Some(error), ..
        }) => {
            assert_eq!(input, "road trip", "the typed name is kept so it can be fixed");
            assert!(error.ends_with("already exists"), "unexpected error: {}", error);
        }
        _ => panic!("a duplicate name must keep the prompt open with an error"),
    }
    let view = render_view(&mut app, 100, 30);
    assert!(view.contains("already exists"), "the error is drawn in the prompt:\n{}", view);

    // Editing clears the error; a name starting with "." is rejected with its own message.
    for _ in 0.."road trip".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { input, error, .. }) => {
            assert!(input.is_empty());
            assert!(error.is_none());
        }
        _ => panic!("Backspace closed the prompt"),
    }
    type_text(&mut app, ".hidden");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { error: Some(error), .. }) => {
            assert_eq!(error, "Name can't start with .")
        }
        _ => panic!("an invalid name must keep the prompt open"),
    }

    // Esc cancels without creating anything.
    assert!(!press(&mut app, KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Road Trip"]);
    assert_eq!(app.status_message, "Cancelled");

    // Ctrl+C still quits from inside the prompt.
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_rename_prefills_current_name_and_cursor_follows() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("rename");
    seed(&mut app, "Alpha", &[temp_dir.join("a.flac")]);
    seed(&mut app, "Beta", &[]);
    seed(&mut app, "Charlie", &[]);
    app.playlists.list_cursor = 0;

    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename { old },
            input,
            error,
        }) => {
            assert_eq!(old, "Alpha");
            assert_eq!(input, "Alpha", "the prompt starts with the current name");
            assert!(error.is_none());
        }
        _ => panic!("R should open the rename prompt"),
    }

    for _ in 0.."Alpha".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    type_text(&mut app, "Zed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Beta", "Charlie", "Zed"]);
    assert_eq!(app.playlists.list_cursor, 2, "the cursor follows the renamed playlist");
    assert_eq!(app.playlists.store.get("Zed").unwrap().entries.len(), 1);
    assert_eq!(app.status_message, "Renamed \"Alpha\" to \"Zed\"");

    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Beta.m3u8", "Charlie.m3u8", "Zed.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_delete_asks_first_and_clamps_cursor() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("delete");
    seed(&mut app, "Alpha", &[]);
    seed(&mut app, "Road Trip", &[temp_dir.join("a.flac"), temp_dir.join("b.flac")]);
    seed(&mut app, "Zulu", &[]);
    app.playlists.list_cursor = 1;

    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    match &app.playlists.modal {
        Some(PlaylistModal::ConfirmDelete { name, count }) => {
            assert_eq!(name, "Road Trip");
            assert_eq!(*count, 2);
        }
        _ => panic!("D should ask before deleting"),
    }
    let view = render_view(&mut app, 100, 30);
    assert!(
        view.contains("Delete \"Road Trip\" (2 tracks)? y/n"),
        "confirmation text:\n{}",
        view
    );

    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Alpha", "Zulu"]);
    assert_eq!(app.playlists.list_cursor, 1, "the cursor stays on the same row (now Zulu)");
    assert_eq!(app.status_message, "Deleted \"Road Trip\"");
    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Alpha.m3u8", "Zulu.m3u8"]);

    // Deleting the last row moves the cursor up to the new last row (Review Focus 5).
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(playlist_names(&app), vec!["Alpha"]);
    assert_eq!(app.playlists.list_cursor, 0);

    // A one-track playlist reads "1 track", not "1 tracks".
    seed(&mut app, "Solo", &[temp_dir.join("s.flac")]);
    app.playlists.list_cursor = 1;
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    let view = render_view(&mut app, 100, 30);
    assert!(view.contains("Delete \"Solo\" (1 track)? y/n"), "confirmation text:\n{}", view);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(playlist_names(&app), vec!["Alpha"]);
    assert_eq!(app.playlists.list_cursor, 0);

    // Deleting the only playlist leaves an empty list with the cursor at 0.
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(playlist_names(&app).is_empty());
    assert_eq!(app.playlists.list_cursor, 0);

    // With nothing selected, D does nothing.
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    assert!(app.playlists.modal.is_none());
    assert!(app.persistence.flush());
    assert!(playlist_files().is_empty());

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_delete_is_cancelled_by_any_key_other_than_y() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("keep");
    seed(&mut app, "Keep Me", &[temp_dir.join("a.flac")]);
    app.playlists.list_cursor = 0;

    for (code, modifiers) in [
        (KeyCode::Char('n'), KeyModifiers::NONE),
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('Y'), KeyModifiers::SHIFT),
    ] {
        press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
        assert!(
            matches!(app.playlists.modal, Some(PlaylistModal::ConfirmDelete { .. })),
            "D should ask before deleting"
        );
        assert!(!press(&mut app, code, modifiers), "{:?} must not quit", code);
        assert!(app.playlists.modal.is_none(), "{:?} closes the confirmation", code);
        assert_eq!(playlist_names(&app), vec!["Keep Me"], "{:?} must not delete", code);
    }
    assert_eq!(app.status_message, "Kept \"Keep Me\"");
    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Keep Me.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_create_is_refused_while_playlists_load() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("loading");
    // A store that has not received the startup load yet.
    app.playlists.store = PlaylistStore::new(paths::get_playlists_dir(), app.persistence.handle());

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(app.playlists.modal.is_none(), "no prompt while loading");
    assert_eq!(app.status_message, "Playlists are still loading");

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_edit -- --test-threads=1
```
Expected: the file compiles and all 6 tests fail. Before this task, `c`, `R` and `D` do nothing on tab 5:
```
test test_create_is_refused_while_playlists_load ... FAILED
test test_create_prompt_types_q_and_digits_and_selects_new_playlist ... FAILED
test test_delete_asks_first_and_clamps_cursor ... FAILED
test test_delete_is_cancelled_by_any_key_other_than_y ... FAILED
test test_name_prompt_shows_errors_and_stays_open ... FAILED
test test_rename_prefills_current_name_and_cursor_follows ... FAILED
```
The panics read `c should open the name prompt`, `R should open the rename prompt` and `D should ask before deleting`. The loading test fails on `assert_eq!(app.status_message, "Playlists are still loading")`.

- [ ] **Step 3: Check the imports in `src/ui/playlists.rs`**

Run `grep -n '^use' src/ui/playlists.rs`. Task 4's signatures already need the lines below. Add any that are missing, and do not add one twice:
```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::AppUI;
```
Keep `KeyEvent` in the crossterm import. The signature `handle_playlist_modal_key(&mut self, key: KeyEvent)` in Step 5 uses it, and Task 6's `handle_add_picker_key` uses it too, so clippy reports no unused import. `MoveDir` and `StoreError` are always written as `crate::playlist_store::...` below, so they need no new import.

- [ ] **Step 4: Dispatch the editing keys from `handle_playlists_tab_key`**

In `src/ui/playlists.rs`, keep Task 4's signature line exactly as it is, including `pub(super)`. `handle_tab_key_with_modifiers` in `src/ui/mod.rs` calls this method. Change only the body: make the dispatch its first statement and leave the rest of the function unchanged:
```rust
    pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if self.handle_playlist_edit_key(code, modifiers) {
            return;
        }
        // ... Task 4's body continues here unchanged (clamp_cursors, then the navigation match)
```

- [ ] **Step 5: Replace the bodies of `handle_playlist_modal_key` and `render_playlist_modal`**

In `src/ui/playlists.rs`, replace the whole of Task 4's `pub fn handle_playlist_modal_key`, which only quit on Ctrl+C and closed the modal on Esc. Keep its signature:
```rust
    /// Keys while a playlist modal is open. Returns true when the app should quit.
    pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
        }
        let Some(modal) = self.playlists.modal.take() else {
            return Ok(false);
        };
        match modal {
            // Like the startup-home editor: every character is text, q and digits included.
            PlaylistModal::NamePrompt {
                purpose,
                mut input,
                error,
            } => match key.code {
                KeyCode::Esc => self.status_message = "Cancelled".to_string(),
                KeyCode::Enter => self.submit_name_prompt(purpose, input),
                KeyCode::Backspace => {
                    input.pop();
                    self.playlists.modal = Some(PlaylistModal::NamePrompt {
                        purpose,
                        input,
                        error: None,
                    });
                }
                KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
                    input.push(c);
                    self.playlists.modal = Some(PlaylistModal::NamePrompt {
                        purpose,
                        input,
                        error: None,
                    });
                }
                _ => self.playlists.modal = Some(PlaylistModal::NamePrompt { purpose, input, error }),
            },
            // Only `y` deletes; any other key cancels.
            PlaylistModal::ConfirmDelete { name, .. } => {
                if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
                    self.delete_playlist(&name);
                } else {
                    self.status_message = format!("Kept \"{}\"", name);
                }
            }
            modal @ PlaylistModal::AddPicker { .. } => {
                if key.code != KeyCode::Esc {
                    self.playlists.modal = Some(modal);
                }
            }
        }
        Ok(false)
    }
```

Then replace the whole of Task 4's `render_playlist_modal`. Keep it `pub(super)`, because `render()` in `src/ui/mod.rs` calls it:
```rust
    pub(super) fn render_playlist_modal(&self, f: &mut Frame) {
        match &self.playlists.modal {
            Some(PlaylistModal::NamePrompt { purpose, input, error }) => {
                render_name_prompt(f, purpose, input, error.as_deref())
            }
            Some(PlaylistModal::ConfirmDelete { name, count }) => render_confirm_delete(f, name, *count),
            Some(PlaylistModal::AddPicker { .. }) | None => {}
        }
    }
```

- [ ] **Step 6: Add the editing methods and the prompt rendering**

Append to the end of `src/ui/playlists.rs`:
```rust
// Editing: name prompt (create / rename), delete confirmation, moving and removing tracks.
impl AppUI {
    /// Tab 5 editing keys. Returns true when the key was handled; anything else falls through
    /// to the navigation keys in `handle_playlists_tab_key`.
    fn handle_playlist_edit_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let tracks_focus = self.playlists.tracks_focus;
        match (code, modifiers) {
            (KeyCode::Char('c'), KeyModifiers::NONE) if !tracks_focus => self.open_create_prompt(),
            (KeyCode::Char('R'), _) if !tracks_focus => self.open_rename_prompt(),
            (KeyCode::Char('D'), _) if !tracks_focus => self.open_delete_confirm(),
            _ => return false,
        }
        true
    }

    fn open_create_prompt(&mut self) {
        // Creating before the startup load arrives would let it overwrite the files on disk.
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        self.playlists.modal = Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Create,
            input: String::new(),
            error: None,
        });
    }

    fn open_rename_prompt(&mut self) {
        let Some(old) = self.playlists.selected_name() else {
            return;
        };
        self.playlists.modal = Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename { old: old.clone() },
            input: old,
            error: None,
        });
    }

    fn open_delete_confirm(&mut self) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let count = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        self.playlists.modal = Some(PlaylistModal::ConfirmDelete { name, count });
    }

    /// Apply a confirmed name prompt. An invalid or duplicate name puts the prompt back with
    /// the error so the user can fix it.
    fn submit_name_prompt(&mut self, purpose: NamePurpose, input: String) {
        let result = match &purpose {
            NamePurpose::Rename { old } => self.playlists.store.rename(old, &input),
            NamePurpose::Create | NamePurpose::CreateAndAdd(_) => self.playlists.store.create(&input),
        };
        let name = match result {
            Ok(name) => name,
            Err(e) => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose,
                    input,
                    error: Some(e.to_string()),
                });
                return;
            }
        };
        // The store re-sorts on create and rename: keep the cursor on the playlist just named.
        if let Some(index) = self.playlists.store.index_of(&name) {
            self.playlists.list_cursor = index;
        }
        match purpose {
            NamePurpose::Rename { old } => {
                self.status_message = format!("Renamed \"{}\" to \"{}\"", old, name);
            }
            NamePurpose::Create | NamePurpose::CreateAndAdd(_) => {
                self.playlists.track_cursor = 0;
                self.status_message = format!("Created \"{}\"", name);
            }
        }
    }

    fn delete_playlist(&mut self, name: &str) {
        match self.playlists.store.delete(name) {
            Ok(()) => {
                // Stay on the same row, or move up to the new last row when the last one went.
                let len = self.playlists.store.playlists().len();
                self.playlists.list_cursor = self.playlists.list_cursor.min(len.saturating_sub(1));
                self.playlists.track_cursor = 0;
                self.status_message = format!("Deleted \"{}\"", name);
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }
}

/// A centred box `height` rows tall and at most 64 columns wide for the playlist prompts.
fn playlist_prompt_rect(r: Rect, height: u16) -> Rect {
    let width = 64.min(r.width.saturating_sub(4));
    let height = height.min(r.height);
    let x = r.x + r.width.saturating_sub(width) / 2;
    let y = r.y + r.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn render_name_prompt(f: &mut Frame, purpose: &NamePurpose, input: &str, error: Option<&str>) {
    let area = playlist_prompt_rect(f.area(), 5);
    f.render_widget(Clear, area);

    let title = match purpose {
        NamePurpose::Create | NamePurpose::CreateAndAdd(_) => " NEW PLAYLIST ".to_string(),
        NamePurpose::Rename { old } => format!(" RENAME \"{}\" ", old),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    // The error replaces the key hint so the prompt keeps its height.
    let message = match error {
        Some(err) => Line::from(Span::styled(err.to_string(), Style::default().fg(Color::Red))),
        None => Line::from(Span::styled(
            "[Enter] Save   [Esc] Cancel",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Name: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{}█", input), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        message,
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_confirm_delete(f: &mut Frame, name: &str, count: usize) {
    let area = playlist_prompt_rect(f.area(), 5);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " DELETE PLAYLIST ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Black));
    let lines = vec![
        Line::from(Span::styled(
            format!("Delete \"{}\" ({})? y/n", name, track_count_label(count)),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[y] Delete   any other key: Cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}
```

- [ ] **Step 7: Run the tests and confirm they pass**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_edit -- --test-threads=1
```
Expected: `test result: ok. 6 passed; 0 failed`.

Then run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_ui -- --test-threads=1
```
Expected: all of Task 4's tests still pass. The new name prompt types `1` and `q` as text instead of ignoring them. `test_playlist_modal_takes_keys_before_global_hotkeys` still holds for these reasons:
- The tab stays at 4 and the modal stays open.
- Esc closes the prompt.
- The prompt title is still `NEW PLAYLIST`.
- The confirmation still reads `Delete "Road Trip" (24 tracks)? y/n`.
- Ctrl+C still quits.

- [ ] **Step 8: Write the failing tests for moving and removing tracks and for the help lines**

In `tests/test_playlists_edit.rs`, change the import
```rust
use quickstream::playlist::PlaylistEntry;
```
to
```rust
use quickstream::playlist::{parse_m3u8, PlaylistEntry};
```
Add this helper below `playlist_names`:
```rust
fn track_paths(app: &AppUI, name: &str) -> Vec<PathBuf> {
    app.playlists
        .store
        .get(name)
        .unwrap()
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect()
}
```
Append these tests:
```rust
#[test]
fn test_shift_j_k_move_track_and_cursor_follows() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("move");
    let (a, b, c) = (temp_dir.join("a.flac"), temp_dir.join("b.flac"), temp_dir.join("c.flac"));
    seed(&mut app, "Mix", &[a.clone(), b.clone(), c.clone()]);
    app.playlists.list_cursor = 0;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;

    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![b.clone(), a.clone(), c.clone()],
        "J should move the track down"
    );
    assert_eq!(app.playlists.track_cursor, 1, "the cursor follows the moved track");

    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b.clone(), c.clone(), a.clone()]);
    assert_eq!(app.playlists.track_cursor, 2);

    // Moves do not wrap around.
    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b.clone(), c.clone(), a.clone()]);
    assert_eq!(app.playlists.track_cursor, 2);

    press(&mut app, KeyCode::Char('K'), KeyModifiers::SHIFT);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![b.clone(), a.clone(), c.clone()],
        "K should move the track up"
    );
    assert_eq!(app.playlists.track_cursor, 1);

    // The file on disk has the same order as memory.
    assert!(app.persistence.flush());
    let dir = paths::get_playlists_dir();
    let text = fs::read_to_string(dir.join("Mix.m3u8")).unwrap();
    let on_disk: Vec<PathBuf> = parse_m3u8("Mix", &text, &dir)
        .entries
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(on_disk, vec![b.clone(), a.clone(), c.clone()]);

    // In the playlists pane, J does not reorder tracks.
    app.playlists.tracks_focus = false;
    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b, a, c]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_d_x_delete_remove_tracks_and_clamp_cursor() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("remove");
    let (a, b, c) = (temp_dir.join("a.flac"), temp_dir.join("b.flac"), temp_dir.join("c.flac"));
    seed(&mut app, "Mix", &[a.clone(), b.clone(), c]);
    app.playlists.list_cursor = 0;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 2;

    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![a.clone(), b],
        "d should remove the selected track"
    );
    assert_eq!(app.playlists.track_cursor, 1, "the cursor moves up to the new last row");
    assert_eq!(app.status_message, "Removed \"c\" from Mix");

    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(track_paths(&app, "Mix"), vec![a]);
    assert_eq!(app.playlists.track_cursor, 0);

    press(&mut app, KeyCode::Delete, KeyModifiers::NONE);
    assert!(track_paths(&app, "Mix").is_empty());
    assert_eq!(app.playlists.track_cursor, 0);

    // Removing from an empty playlist is a no-op.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert!(track_paths(&app, "Mix").is_empty());

    // Removing every track keeps the (now empty) playlist file.
    assert!(app.persistence.flush());
    let dir = paths::get_playlists_dir();
    let text = fs::read_to_string(dir.join("Mix.m3u8")).unwrap();
    assert!(parse_m3u8("Mix", &text, &dir).entries.is_empty());
    assert_eq!(playlist_names(&app), vec!["Mix"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lists_playlist_editing_keys() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("help");
    press(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
    assert!(app.show_help_modal);

    let view = render_view(&mut app, 120, 60);
    for text in ["New playlist (tab 5)", "Rename / Delete playlist", "Move track down / up"] {
        assert!(view.contains(text), "help should list {:?}:\n{}", text, view);
    }
    // Small terminals still render without panicking.
    render_view(&mut app, 70, 20);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}
```

- [ ] **Step 9: Run the tests and confirm the new ones fail**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_edit -- --test-threads=1
```
Expected: the 6 tests from Step 1 pass and the 3 new ones fail:
```
test test_d_x_delete_remove_tracks_and_clamp_cursor ... FAILED
test test_help_modal_lists_playlist_editing_keys ... FAILED
test test_shift_j_k_move_track_and_cursor_follows ... FAILED
test result: FAILED. 6 passed; 3 failed
```
The messages are `J should move the track down`, `d should remove the selected track` and `help should list "New playlist (tab 5)"`.

- [ ] **Step 10: Add the move and remove keys**

In `src/ui/playlists.rs`, inside `handle_playlist_edit_key`, put these arms just before `_ => return false,`:
```rust
            (KeyCode::Char('D'), _) if !tracks_focus => self.open_delete_confirm(),
            (KeyCode::Char('K'), _) if tracks_focus => self.move_selected_track(crate::playlist_store::MoveDir::Up),
            (KeyCode::Char('J'), _) if tracks_focus => {
                self.move_selected_track(crate::playlist_store::MoveDir::Down)
            }
            (KeyCode::Char('d') | KeyCode::Char('x'), KeyModifiers::NONE) | (KeyCode::Delete, _) if tracks_focus => {
                self.remove_selected_track()
            }
            _ => return false,
```
The first line (`D`) is the existing anchor. It stays as it is.

In the same `impl AppUI` block (the one added in Step 6), add these methods after `delete_playlist`:
```rust
    /// Move the selected track one row; the cursor stays on it. Moves do not wrap around.
    fn move_selected_track(&mut self, dir: crate::playlist_store::MoveDir) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let len = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        if self.playlists.track_cursor >= len {
            return;
        }
        match self.playlists.store.move_entry(&name, self.playlists.track_cursor, dir) {
            Ok(index) => self.playlists.track_cursor = index,
            Err(e) => self.status_message = e.to_string(),
        }
    }

    fn remove_selected_track(&mut self) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let len = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        if self.playlists.track_cursor >= len {
            return;
        }
        match self.playlists.store.remove(&name, self.playlists.track_cursor) {
            Ok(entry) => {
                let remaining = len - 1;
                self.playlists.track_cursor = self.playlists.track_cursor.min(remaining.saturating_sub(1));
                self.status_message = format!("Removed \"{}\" from {}", entry.display_label(), name);
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }
```

- [ ] **Step 11: Add the help lines**

In `src/ui/mod.rs`, in `render_help_modal`, find the `left_lines` vector. Its last section is Task 4's `PLAYLISTS (TAB 5)`, and the anchor line is:
```rust
            Self::help_key_line("Enter", "Open playlist's tracks"),
```
Directly after that line, and before the closing `];` of `left_lines`, insert:
```rust
            Self::help_key_line("c", "New playlist (tab 5)"),
            Self::help_key_line("R / D", "Rename / Delete playlist"),
            Self::help_key_line("J / K", "Move track down / up"),
```
The existing `d / x` → `Remove track / bookmark` line in `right_lines` already covers removing a track in tab 5.

Do not edit `help_modal_rect` or `HELP_MODAL_HEIGHT`. The left column grows from 20 to 23 lines, and Task 7 adds 1 more for 24. That fits in the 27 rows of the 30-row help modal.

- [ ] **Step 12: Run the tests and confirm they pass**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test --test test_playlists_edit -- --test-threads=1
```
Expected: `test result: ok. 9 passed; 0 failed`.

Then run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1
```
Expected: the whole suite passes, including:
- `test_isolation`: every new test calls `common::isolate()` first.
- `test_playlists_ui::test_playlist_modal_takes_keys_before_global_hotkeys`
- `test_browser::test_help_modal_lifecycle_and_render`

- [ ] **Step 13: Format and lint**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"; cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings
```
Expected: `cargo fmt --check` prints nothing, and clippy reports no warnings or errors. There is no `unused import: KeyEvent`, because `handle_playlist_modal_key` takes `KeyEvent`.

- [ ] **Step 14: Commit**

```bash
git add src/ui/playlists.rs src/ui/mod.rs tests/test_playlists_edit.rs
git commit -m "$(cat <<'EOF'
feat(playlists): tab 5 create, rename, delete, move and remove

Name prompt (c / R) takes every character as text, keeps the prompt open
with the validation error, and moves the cursor to the playlist's sorted
row. D asks "Delete \"<name>\" (<n> tracks)? y/n" and clamps the cursor.
Shift+K / Shift+J move the selected track, d / x / Del remove it.
Help modal lists the new keys.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```


---

### Task 6: Add key L, add picker, last_playlist

**Files:**
- Modify: `src/settings.rs`
  - `struct Settings`: add a field after `pub web_server_lan: bool,`.
  - `impl Default for Settings`: add a field after `web_server_lan: false,`.
- Modify: `src/ui/playlists.rs` (Tasks 4 and 5 created and extended it)
  - `pub struct PlaylistsUi`: add two private channel fields after `    pub exists: HashMap<PathBuf, bool>,`.
  - `PlaylistsUi::new`: create the channel and add the two fields to the `Self { .. }` literal.
  - `AppUI::handle_playlist_modal_key`: add an add-picker dispatch and a create-and-add intercept right after its Ctrl+C quit check.
  - `AppUI::render_playlist_modal`: render the add picker at the top of the function.
  - `AppUI::poll_playlists`: add one call at the end of its body.
  - New items at the end of the file: `PendingAdd::label`, `list_folder_audio_files`, private helpers, and a new `impl super::AppUI` block. The file reuses Task 4's `NEW_PLAYLIST_ROW` and `track_count_label` and does not define them again.
- Modify: `src/ui/mod.rs`
  - `handle_input_key` global hotkey match: add an `L` arm right after the `(KeyCode::Char('M'), _) | (KeyCode::Char('m'), KeyModifiers::SHIFT)` arm and before `_ => { // Tab specific key handling`.
  - `render_help_modal`: add a line to `right_lines` after `Self::help_key_line("a", "Add all songs in folder"),`.
- Test (new file): `tests/test_playlists_add.rs`. It is a separate test binary, so its helpers cannot clash with those in Task 4's `tests/test_playlists_ui.rs`.

**Interfaces:**
- Consumes:
  - Task 1:
    - `crate::playlist::PlaylistEntry { path, duration_secs, artist, title }`
    - `PlaylistEntry::from_metadata(&AudioMetadata) -> PlaylistEntry`
    - `PlaylistEntry::from_path(&Path) -> PlaylistEntry`
    - `paths::get_playlists_dir() -> PathBuf`
  - Task 3:
    - `PlaylistStore::{is_loaded, playlists, get, index_of, create, add}`
    - `StoreError::NotLoaded`, whose `Display` is `"Playlists are still loading"`
    - `AddResult { added, skipped, total }`
  - Task 4:
    - `AppUI.playlists: PlaylistsUi { store, list_cursor, track_cursor, modal, exists, .. }`, where `exists` is declared as `pub exists: HashMap<PathBuf, bool>,` and the file has `use std::collections::HashMap;`
    - `PlaylistModal::{NamePrompt { purpose, input, error }, ConfirmDelete { .. }, AddPicker { pending, cursor }}`
    - `NamePurpose::CreateAndAdd(PendingAdd)`
    - `PendingAdd::{Entries { label, entries }, Folder { label, dir }}`
    - `pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";` (`src/ui/playlists.rs`)
    - `pub fn track_count_label(count: usize) -> String`, which returns `"1 track"` or `"<n> tracks"` (`src/ui/playlists.rs`)
    - `PlaylistsUi::new(PersistHandle)`
    - `AppUI::{start_playlist_load, poll_playlists, handle_playlist_modal_key, render_playlist_modal}`
    - `HELP_MODAL_HEIGHT = 30` in `src/ui/mod.rs`, which leaves 27 content rows in the help modal
    - Three assumptions about the wiring:
      - `AppUI::new` starts the playlist load.
      - `handle_input_key` routes keys to `handle_playlist_modal_key` while `self.playlists.modal.is_some()`.
      - `render()` draws `render_playlist_modal` whenever a playlist modal is open, on every tab.
  - Task 5: in the name prompt, `Char` keys with `NONE` or `SHIFT` modifiers append to `input`, `Backspace` deletes and `Esc` cancels.
  - Existing code:
    - `AppUI::{metadata_cache, queue, queue_focus, queue_cursor, browser_entries, browser_cursor, active_tab, player, settings_mgr, workers, status_message}`
    - `crate::metadata::is_supported_audio_file`
    - `super::centered_rect`
- Produces:
  ```rust
  // src/settings.rs
  pub struct Settings { /* … */ #[serde(default)] pub last_playlist: Option<String> }
  // src/ui/playlists.rs
  impl PendingAdd { pub fn label(&self) -> &str; }
  pub fn list_folder_audio_files(dir: &Path) -> std::io::Result<Vec<PathBuf>>; // blocking, worker only
  impl AppUI { pub fn open_add_picker(&mut self); }
  // private: struct FolderListing; PlaylistsUi.folder_tx / folder_rx;
  //          AppUI::{pending_add_for_selection, playlist_entry_for_path, handle_add_picker_key,
  //          move_add_picker_cursor, confirm_add_picker, confirm_create_and_add, apply_pending_add,
  //          add_entries_to_playlist, start_folder_add, poll_playlist_folder_adds, render_add_picker}
  ```
- Status messages:

  | Case | Message |
  |---|---|
  | Store not loaded | `Playlists are still loading` |
  | `★ Music Homes`, `..` or an empty browser row | `Select a track or folder` |
  | Queue pane with an empty queue | `Queue is empty` |
  | Other tabs, nothing playing or paused | `Nothing is playing` |
  | One track added | `Added "<title>" to <name> (<n> track/tracks)` |
  | Several added, none skipped | `Added <k> to <name> (<n> track/tracks)` |
  | Several added, some skipped | `Added <k> to <name>, skipped <m> already in it` |
  | Nothing new | `Already in <name>` |
  | Folder add in progress | `Reading <folder>…` |
  | Folder could not be read | `Couldn't read <folder>` |
  | Folder has no audio files | `No audio files in <folder>` |

---

- [ ] **Step 1: Pre-flight: confirm the Task 4 and 5 hooks this task builds on**

```bash
cd /Users/jp/Development/quickstream
grep -n "start_playlist_load\|poll_playlists\|handle_playlist_modal_key\|render_playlist_modal" src/ui/mod.rs src/ui/playlists.rs
grep -n "pub exists\|pub fn new(persist\|NEW_PLAYLIST_ROW\|pub fn track_count_label\|^use std::collections::HashMap" src/ui/playlists.rs
grep -n "HELP_MODAL_HEIGHT" src/ui/mod.rs
```
Check the output for each of the following:
- `self.start_playlist_load()` (or `app.start_playlist_load()`) is called inside `AppUI::new`.
- `handle_playlist_modal_key` is called from `handle_input_key` above the `// Global hotkeys` match.
- `render_playlist_modal` is called from `render()` under a condition on `self.playlists.modal.is_some()` alone. It must not be inside the `match self.active_tab` arm for tab 4.
- In `src/ui/playlists.rs`:
  - `use std::collections::HashMap;`
  - `    pub exists: HashMap<PathBuf, bool>,`
  - `pub fn new(persist: crate::persist::PersistHandle) -> Self`
  - `pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";`
  - `pub fn track_count_label(count: usize) -> String`
- `const HELP_MODAL_HEIGHT: u16 = 30;` is in `src/ui/mod.rs` and is used by `render_help_modal`.

The tests below depend on all of these.

- [ ] **Step 2: Write the failing `last_playlist` settings test**

Create `tests/test_playlists_add.rs`:

```rust
//! Adding to playlists with `L`: selection rules, the add picker, folder adds and
//! `last_playlist` (issue #37).

mod common;

use std::fs;
use std::path::PathBuf;

use quickstream::settings::{Settings, SettingsManager};

/// Fresh scratch folder for one test, removed first if a previous run left it behind.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quickstream_playlists_add_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_last_playlist_defaults_to_none_and_round_trips() {
    common::isolate();
    assert_eq!(Settings::default().last_playlist, None);
    let legacy: Settings = serde_json::from_str(
        r#"{"metadata_view":true,"default_device":"local-audio","seek_step_seconds":10.0,"volume_step":5,"queue_mode":"loop","force_transcode":false,"player_backend":"mpv"}"#,
    )
    .unwrap();
    assert_eq!(
        legacy.last_playlist, None,
        "configs saved before playlists existed load with no last playlist"
    );

    let dir = scratch_dir("settings");
    let path = dir.join("config.json");
    let mut mgr = SettingsManager::with_path(path.clone());
    mgr.settings.last_playlist = Some("Road Trip".to_string());
    assert!(mgr.save());
    let loaded = SettingsManager::load_from(&path).expect("settings load");
    assert_eq!(loaded.last_playlist.as_deref(), Some("Road Trip"));
    let _ = fs::remove_dir_all(&dir);
}
```

- [ ] **Step 3: Run the test and confirm that it fails**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
```
Expected: compile error `error[E0609]: no field 'last_playlist' on type 'Settings'`.

- [ ] **Step 4: Add `last_playlist` to `Settings`**

In `src/settings.rs`, replace:
```rust
    #[serde(default)]
    pub web_server_lan: bool,
}
```
with:
```rust
    #[serde(default)]
    pub web_server_lan: bool,
    /// Playlist the add picker (`L`) highlights first: the one last added to.
    #[serde(default)]
    pub last_playlist: Option<String>,
}
```
In `impl Default for Settings`, replace:
```rust
            web_server_lan: false,
        }
```
with:
```rust
            web_server_lan: false,
            last_playlist: None,
        }
```

- [ ] **Step 5: Run the tests and confirm that they pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
cargo test --test test_isolation -- --test-threads=1
```
Expected:
- `test_last_playlist_defaults_to_none_and_round_trips ... ok`.
- `test_isolated_config_starts_from_defaults_not_legacy_files` still passes. The defaults that `common::isolate()` writes now include `"last_playlist": null`.

- [ ] **Step 6: Write the failing folder-listing test**

In `tests/test_playlists_add.rs`, add this import below `use quickstream::settings::{Settings, SettingsManager};`:
```rust
use quickstream::ui::playlists::list_folder_audio_files;
```
Add this test at the end of the file:
```rust
#[test]
fn test_list_folder_audio_files_is_flat_sorted_and_audio_only() {
    common::isolate();
    let dir = scratch_dir("listing");
    for name in [
        "02 Second.flac",
        "01 First.mp3",
        "10 Tenth.WAV",
        "cover.jpg",
        "notes.txt",
        "._01 First.mp3",
    ] {
        fs::write(dir.join(name), b"").unwrap();
    }
    fs::create_dir_all(dir.join("Disc 2")).unwrap();
    fs::write(dir.join("Disc 2").join("01 Nested.flac"), b"").unwrap();

    let files = list_folder_audio_files(&dir).expect("folder is readable");
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // Name order, audio only, no AppleDouble "._" files, nothing from subfolders.
    assert_eq!(names, vec!["01 First.mp3", "02 Second.flac", "10 Tenth.WAV"]);
    assert!(files.iter().all(|p| p.parent() == Some(dir.as_path())));
    assert!(list_folder_audio_files(&dir.join("missing")).is_err());
    let _ = fs::remove_dir_all(&dir);
}
```

- [ ] **Step 7: Run the test and confirm that it fails**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
```
Expected: `error[E0432]: unresolved import 'quickstream::ui::playlists::list_folder_audio_files'`.

- [ ] **Step 8: Implement `list_folder_audio_files`**

Append the function below to `src/ui/playlists.rs`. Task 4 leaves the import as `use std::path::PathBuf;`. Change it to `use std::path::{Path, PathBuf};` and do not add a second `std::path` import.

```rust
/// The audio files directly inside `dir` (not recursive), sorted by name case-insensitively.
/// Blocking: call it on a worker thread, never on the UI thread.
pub fn list_folder_audio_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        // Skip hidden files, including the "._name" AppleDouble files macOS leaves on network shares.
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| crate::metadata::is_supported_audio_file(path))
        .collect();
    files.sort_by_cached_key(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()));
    Ok(files)
}
```

- [ ] **Step 9: Run the tests and confirm that they pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
```
Expected: 2 passed.

- [ ] **Step 10: Write the failing add-key and picker tests**

In `tests/test_playlists_add.rs`, replace the import block with the one below. The block runs from `use std::fs;` through the `list_folder_audio_files` import.
```rust
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::metadata::AudioMetadata;
use quickstream::player::AudioPlayer;
use quickstream::settings::{Settings, SettingsManager};
use quickstream::ui::playlists::{list_folder_audio_files, NamePurpose, PendingAdd, PlaylistModal};
use quickstream::ui::AppUI;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
```
Add these helpers after `scratch_dir`:
```rust
/// App browsing `dir`, with no playlists on disk. Tests in this binary share one isolated
/// config dir, so each one starts by removing the playlists folder.
fn new_app(dir: &Path) -> AppUI {
    let _ = fs::remove_dir_all(quickstream::paths::get_playlists_dir());
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    // No background tag reads: the tests seed the metadata cache themselves.
    settings_mgr.settings.metadata_view = false;
    AppUI::new(Some(dir.to_path_buf()), settings_mgr)
}

/// Poll the playlist channels until `done` holds (about 2 s at most).
fn wait_for(app: &mut AppUI, what: &str, done: impl Fn(&AppUI) -> bool) {
    for _ in 0..200 {
        app.poll_playlists();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", what);
}

fn wait_loaded(app: &mut AppUI) {
    wait_for(app, "playlists to load", |a| a.playlists.store.is_loaded());
}

/// Wait for the browser's directory scan to finish.
fn wait_browser_listed(app: &mut AppUI) {
    for _ in 0..200 {
        app.poll_background_dir_scan();
        if !app.browser_loading {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("browser listing never finished");
}

/// Put the browser cursor on the row for `path` once the listing is complete.
fn select_browser_row(app: &mut AppUI, path: &Path) {
    wait_browser_listed(app);
    app.browser_cursor = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p == path)
        .unwrap_or_else(|| panic!("{} is not in the browser", path.display()));
}

/// Send a key the way a terminal does: uppercase letters carry SHIFT.
fn press(app: &mut AppUI, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Stands in for mpv so `L` outside tab 2 has a now-playing track.
struct StubPlayer {
    playing: bool,
}

impl AudioPlayer for StubPlayer {
    fn play(&mut self, _filepath: &Path) -> anyhow::Result<()> {
        self.playing = true;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.playing = false;
        Ok(())
    }
    fn seek(&mut self, _delta_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn seek_to(&mut self, _position_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_volume(&mut self, _volume: u8) -> anyhow::Result<()> {
        Ok(())
    }
    fn volume(&self) -> u8 {
        50
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn is_paused(&self) -> bool {
        false
    }
    fn position_seconds(&self) -> f64 {
        0.0
    }
    fn duration_seconds(&self) -> f64 {
        0.0
    }
}
```
Add these tests at the end of the file:
```rust
#[test]
fn test_add_key_refused_while_loading_and_on_header_rows() {
    common::isolate();
    let dir = scratch_dir("headers");
    let mut app = new_app(&dir);

    // The startup load result is applied by poll_playlists, which has not run yet.
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Playlists are still loading");

    wait_loaded(&mut app);
    wait_browser_listed(&mut app);
    assert_eq!(app.browser_entries[0].0, "★ Music Homes");
    assert!(app.browser_entries[1].0.starts_with(".."));
    for row in [0, 1] {
        app.status_message.clear();
        app.browser_cursor = row;
        press(&mut app, KeyCode::Char('L'));
        assert!(app.playlists.modal.is_none(), "row {} is not addable", row);
        assert_eq!(app.status_message, "Select a track or folder");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_on_browser_track_adds_via_picker_and_remembers_playlist() {
    common::isolate();
    let dir = scratch_dir("track");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Road Trip").unwrap();
    app.metadata_cache.insert(AudioMetadata {
        filepath: track.clone(),
        filename: "Kerala.flac".to_string(),
        title: "Kerala".to_string(),
        artist: "Bonobo".to_string(),
        duration_seconds: 245.4,
        ..Default::default()
    });
    select_browser_row(&mut app, &track);

    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Entries { label, entries },
            cursor,
        }) => {
            assert_eq!(label, "Kerala");
            assert_eq!(*cursor, 0, "no last playlist yet: first row");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, track);
            assert_eq!(entries[0].artist, "Bonobo", "entry comes from the metadata cache");
            assert_eq!(entries[0].duration_secs, 245);
        }
        _ => panic!("L on a track should open the add picker"),
    }
    press(&mut app, KeyCode::Down); // Road Trip
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Added \"Kerala\" to Road Trip (1 track)");
    let road_trip = app.playlists.store.get("Road Trip").unwrap();
    assert_eq!(road_trip.entries.len(), 1);
    assert_eq!(road_trip.entries[0].path, track);
    assert!(app.playlists.store.get("Alpha").unwrap().entries.is_empty());
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));
    assert!(app.persistence.flush());
    let saved = SettingsManager::load_from(&dir.join("config.json")).expect("settings saved");
    assert_eq!(saved.last_playlist.as_deref(), Some("Road Trip"));

    // Next time the picker starts on Road Trip; adding the same track again changes nothing.
    press(&mut app, KeyCode::Char('L'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::AddPicker { cursor: 1, .. })
    ));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Already in Road Trip");
    assert_eq!(app.playlists.store.get("Road Trip").unwrap().entries.len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_picker_wraps_cancels_and_falls_back_when_last_playlist_is_gone() {
    common::isolate();
    let dir = scratch_dir("picker");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Road Trip").unwrap();
    select_browser_row(&mut app, &track);

    // last_playlist matches case-insensitively, like every playlist name.
    app.settings_mgr.settings.last_playlist = Some("road trip".to_string());
    press(&mut app, KeyCode::Char('L'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::AddPicker { cursor: 1, .. })
    ));
    press(&mut app, KeyCode::Esc);
    assert!(app.playlists.modal.is_none());

    // A last playlist that no longer exists highlights the first row.
    app.settings_mgr.settings.last_playlist = Some("Deleted Mix".to_string());
    press(&mut app, KeyCode::Char('L'));
    let cursor = |app: &AppUI| match &app.playlists.modal {
        Some(PlaylistModal::AddPicker { cursor, .. }) => *cursor,
        _ => panic!("add picker should be open"),
    };
    assert_eq!(cursor(&app), 0);
    // Rows: Alpha, Road Trip, "+ New playlist…"; movement wraps both ways.
    press(&mut app, KeyCode::Up);
    assert_eq!(cursor(&app), 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(cursor(&app), 0);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(cursor(&app), 2);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(cursor(&app), 0);
    press(&mut app, KeyCode::Char('q'));
    assert!(app.playlists.modal.is_none());

    assert!(app.playlists.store.playlists().iter().all(|p| p.entries.is_empty()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_new_playlist_row_creates_the_playlist_and_adds() {
    common::isolate();
    let dir = scratch_dir("create_add");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Zulu").unwrap();
    select_browser_row(&mut app, &track);

    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Up); // wraps to "+ New playlist…"
    press(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::CreateAndAdd(_),
            ..
        })
    ));
    type_text(&mut app, "Road Trip");
    press(&mut app, KeyCode::Enter);

    assert!(app.playlists.modal.is_none());
    let road_trip = app.playlists.store.get("Road Trip").expect("playlist created");
    assert_eq!(road_trip.entries.len(), 1);
    // No cached tags: the title falls back to the file stem.
    assert_eq!(road_trip.entries[0].title, "Kerala");
    assert_eq!(road_trip.entries[0].duration_secs, -1);
    assert_eq!(app.status_message, "Added \"Kerala\" to Road Trip (1 track)");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));
    // The tab 5 list cursor follows the new playlist to its sorted position.
    assert_eq!(app.playlists.list_cursor, 1);

    // A duplicate name keeps the prompt open with the error and the pending track.
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down); // Road Trip (1) -> Zulu (2) -> "+ New playlist…" (3)
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "road trip");
    press(&mut app, KeyCode::Enter);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::CreateAndAdd(PendingAdd::Entries { entries, .. }),
            error: Some(error),
            ..
        }) => {
            assert!(error.contains("already exists"), "{}", error);
            assert_eq!(entries.len(), 1);
        }
        _ => panic!("duplicate name should keep the prompt open with an error"),
    }
    press(&mut app, KeyCode::Esc);
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.playlists.store.playlists().len(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_on_folder_adds_its_audio_files_in_name_order() {
    common::isolate();
    let dir = scratch_dir("folder");
    let album = dir.join("Album");
    fs::create_dir_all(album.join("Bonus")).unwrap();
    for name in ["02 Paranoid Android.flac", "01 Airbag.flac", "folder.jpg"] {
        fs::write(album.join(name), b"").unwrap();
    }
    fs::write(album.join("Bonus").join("03 Nested.flac"), b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();
    select_browser_row(&mut app, &album);

    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Folder { label, dir: folder },
            ..
        }) => {
            assert_eq!(label, "Album (folder)");
            assert_eq!(folder, &album);
        }
        _ => panic!("L on a folder should open the add picker for the folder"),
    }
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.modal.is_none());
    // The listing runs on a worker; nothing is added until poll_playlists sees the result.
    assert_eq!(app.status_message, "Reading Album…");
    wait_for(&mut app, "folder add", |a| {
        a.playlists
            .store
            .get("Road Trip")
            .is_some_and(|p| p.entries.len() == 2)
    });
    let paths: Vec<PathBuf> = app.playlists.store.get("Road Trip").unwrap().entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        paths,
        vec![album.join("01 Airbag.flac"), album.join("02 Paranoid Android.flac")]
    );
    assert_eq!(app.status_message, "Added 2 to Road Trip (2 tracks)");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));

    // Adding the folder again after a new file appears adds only the new file.
    fs::write(album.join("00 Intro.flac"), b"").unwrap();
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "second folder add", |a| a.status_message.contains("skipped"));
    assert_eq!(app.status_message, "Added 1 to Road Trip, skipped 2 already in it");
    assert_eq!(app.playlists.store.get("Road Trip").unwrap().entries.len(), 3);

    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "third folder add", |a| a.status_message == "Already in Road Trip");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_folder_that_cannot_be_read_reports_it() {
    common::isolate();
    let dir = scratch_dir("unreadable");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();

    // The folder was listed in the browser, then the share went away.
    let gone = dir.join("Unplugged");
    app.playlists.modal = Some(PlaylistModal::AddPicker {
        pending: PendingAdd::Folder {
            label: "Unplugged (folder)".to_string(),
            dir: gone,
        },
        cursor: 0,
    });
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "folder read error", |a| a.status_message == "Couldn't read Unplugged");
    assert!(app.playlists.store.get("Road Trip").unwrap().entries.is_empty());
    assert_eq!(app.settings_mgr.settings.last_playlist, None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_uses_queue_row_and_now_playing_track() {
    common::isolate();
    let dir = scratch_dir("queue_now_playing");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();

    app.queue_focus = true;
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Queue is empty");

    let airbag = AudioMetadata {
        filepath: dir.join("01 Airbag.flac"),
        filename: "01 Airbag.flac".to_string(),
        title: "Airbag".to_string(),
        artist: "Radiohead".to_string(),
        duration_seconds: 284.4,
        ..Default::default()
    };
    let lucky = AudioMetadata {
        filepath: dir.join("11 Lucky.flac"),
        filename: "11 Lucky.flac".to_string(),
        title: "Lucky".to_string(),
        artist: "Radiohead".to_string(),
        duration_seconds: 259.0,
        ..Default::default()
    };
    app.queue.add_track(airbag.clone());
    app.queue.add_track(lucky.clone());

    // Queue pane: the selected queue row, with its tags.
    app.queue_cursor = 0;
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Added \"Airbag\" to Road Trip (1 track)");
    let entry = app.playlists.store.get("Road Trip").unwrap().entries[0].clone();
    assert_eq!(entry.path, airbag.filepath);
    assert_eq!(entry.artist, "Radiohead");
    assert_eq!(entry.title, "Airbag");
    assert_eq!(entry.duration_secs, 284);

    // Any other tab: the now-playing track, and only while something is playing.
    app.active_tab = 0;
    app.queue.current_index = 1;
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Nothing is playing");

    app.player = Box::new(StubPlayer { playing: true });
    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Entries { label, .. },
            ..
        }) => assert_eq!(label, "Lucky"),
        _ => panic!("L while playing should open the add picker for the current track"),
    }
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Added \"Lucky\" to Road Trip (2 tracks)");
    assert_eq!(app.playlists.store.get("Road Trip").unwrap().entries[1].path, lucky.filepath);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_picker_renders_header_rows_and_new_playlist_row() {
    common::isolate();
    let dir = scratch_dir("render");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();
    app.queue.add_track(AudioMetadata {
        filepath: dir.join("01 Airbag.flac"),
        title: "Airbag".to_string(),
        artist: "Radiohead".to_string(),
        ..Default::default()
    });
    app.queue_focus = true;
    press(&mut app, KeyCode::Char('L'));

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let view = terminal.backend().to_string();
    assert!(view.contains("ADD TO PLAYLIST"), "{}", view);
    assert!(view.contains("Adding: Airbag"), "{}", view);
    assert!(view.contains("Road Trip  (0 tracks)"), "{}", view);
    assert!(view.contains("+ New playlist…"), "{}", view);
    let _ = fs::remove_dir_all(&dir);
}
```

- [ ] **Step 11: Run the tests and confirm that the new ones fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
```
Expected: the file compiles, the 2 earlier tests pass, and the 8 new tests FAIL because `L` has no binding yet. For example:
- `test_add_key_refused_while_loading_and_on_header_rows` fails with `assertion 'left == right' failed`, where `left` is `"Welcome to Quickstream (Rust)"` and `right` is `"Playlists are still loading"`.
- `test_add_key_on_browser_track_adds_via_picker_and_remembers_playlist` panics with `L on a track should open the add picker`.

- [ ] **Step 12a: Add the folder channel to `PlaylistsUi`**

In `src/ui/playlists.rs`, in `pub struct PlaylistsUi`, replace:
```rust
    pub exists: HashMap<PathBuf, bool>,
```
with:
```rust
    pub exists: HashMap<PathBuf, bool>,
    /// Folder listings for folder adds (`L` on a folder), sent by `playlist-folder` workers.
    folder_tx: mpsc::Sender<FolderListing>,
    folder_rx: mpsc::Receiver<FolderListing>,
```
`std::sync::mpsc` is already imported at the top of the file. Task 4 uses it for the load channel.

In `PlaylistsUi::new`, add this as the first statement of the body:
```rust
        let (folder_tx, folder_rx) = mpsc::channel();
```
Then add these two fields to its `Self { .. }` literal, after `exists: …,`:
```rust
            folder_tx,
            folder_rx,
```

- [ ] **Step 12b: Add the picker helpers and the add flow**

The top of `src/ui/playlists.rs` already has the imports this code needs:
- `use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};`
- `use ratatui::prelude::*;`
- `use ratatui::widgets::*;`
- `use std::path::{Path, PathBuf};` (from Step 8)

Do not add duplicates.

The code below uses Task 4's `NEW_PLAYLIST_ROW` and `track_count_label`, which are already defined in this file. Do not redefine them. Append:

```rust
/// Status for `L` on a browser row that is neither a track nor a folder (`★ Music Homes`, `..`).
const SELECT_TRACK_OR_FOLDER: &str = "Select a track or folder";

/// A folder listing for a folder add, sent from the worker to the UI thread.
#[derive(Debug)]
struct FolderListing {
    playlist: String,
    dir: PathBuf,
    result: std::io::Result<Vec<PathBuf>>,
}

impl PendingAdd {
    /// What the add picker's header shows: the track title, or `<folder> (folder)`.
    pub fn label(&self) -> &str {
        match self {
            PendingAdd::Entries { label, .. } | PendingAdd::Folder { label, .. } => label,
        }
    }
}

/// Pending add of one track; the picker header shows its title.
fn pending_track(entry: PlaylistEntry) -> PendingAdd {
    PendingAdd::Entries {
        label: entry.title.clone(),
        entries: vec![entry],
    }
}

/// Last path component of `dir` for messages, or the whole path for a root.
fn folder_display_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// Status line after an add: `Added "Kerala" to Road Trip (25 tracks)`, `Already in Road Trip`
/// or `Added 9 to Road Trip, skipped 3 already in it`.
fn add_result_message(playlist: &str, single_title: Option<&str>, added: usize, skipped: usize, total: usize) -> String {
    if added == 0 {
        format!("Already in {}", playlist)
    } else if let Some(title) = single_title {
        format!("Added \"{}\" to {} ({})", title, playlist, track_count_label(total))
    } else if skipped == 0 {
        format!("Added {} to {} ({})", added, playlist, track_count_label(total))
    } else {
        format!("Added {} to {}, skipped {} already in it", added, playlist, skipped)
    }
}

impl super::AppUI {
    /// `L`: open the add picker for the current selection. On tab 2 that is the focused pane's
    /// row (a browser track or folder, or a queue track); on every other tab it is the
    /// now-playing track.
    pub fn open_add_picker(&mut self) {
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        let pending = match self.pending_add_for_selection() {
            Ok(pending) => pending,
            Err(message) => {
                self.status_message = message;
                return;
            }
        };
        // Start on the playlist last added to; the first row if it is unset or gone.
        let cursor = self
            .settings_mgr
            .settings
            .last_playlist
            .as_deref()
            .and_then(|name| self.playlists.store.index_of(name))
            .unwrap_or(0);
        self.playlists.modal = Some(PlaylistModal::AddPicker { pending, cursor });
    }

    /// What `L` would add, or the status message explaining why nothing can be added.
    fn pending_add_for_selection(&self) -> Result<PendingAdd, String> {
        if self.active_tab == 1 && self.queue_focus {
            return match self.queue.tracks.get(self.queue_cursor) {
                Some(track) => Ok(pending_track(PlaylistEntry::from_metadata(track))),
                None => Err("Queue is empty".to_string()),
            };
        }
        if self.active_tab == 1 {
            let Some((label, path, is_dir)) = self.browser_entries.get(self.browser_cursor) else {
                return Err(SELECT_TRACK_OR_FOLDER.to_string());
            };
            if !*is_dir {
                return Ok(pending_track(self.playlist_entry_for_path(path)));
            }
            // Folder rows (also bookmarked Music Homes) are labelled "📁 name/"; the
            // "★ Music Homes" and ".." rows are navigation, not folders to add.
            if label.starts_with("📁 ") {
                return Ok(PendingAdd::Folder {
                    label: format!("{} (folder)", folder_display_name(path)),
                    dir: path.clone(),
                });
            }
            return Err(SELECT_TRACK_OR_FOLDER.to_string());
        }
        if self.player.is_playing() || self.player.is_paused() {
            if let Some(track) = self.queue.current_track() {
                return Ok(pending_track(PlaylistEntry::from_metadata(track)));
            }
        }
        Err("Nothing is playing".to_string())
    }

    /// Playlist entry for a file from metadata the app already holds. Never reads the file.
    fn playlist_entry_for_path(&self, path: &Path) -> PlaylistEntry {
        match self.metadata_cache.get(path) {
            Some(meta) => PlaylistEntry::from_metadata(&meta),
            None => PlaylistEntry::from_path(path),
        }
    }

    /// Keys while the add picker is open: j/k/arrows move (wrapping), Enter adds, Esc/q cancel.
    fn handle_add_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.playlists.modal = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_add_picker_cursor(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_add_picker_cursor(true),
            KeyCode::Enter => self.confirm_add_picker(),
            _ => {}
        }
    }

    fn move_add_picker_cursor(&mut self, down: bool) {
        // One row per playlist, then "+ New playlist…".
        let rows = self.playlists.store.playlists().len() + 1;
        if let Some(PlaylistModal::AddPicker { cursor, .. }) = self.playlists.modal.as_mut() {
            *cursor = if down {
                (*cursor + 1) % rows
            } else {
                (*cursor + rows - 1) % rows
            };
        }
    }

    /// Enter in the add picker: add to the highlighted playlist, or open the name prompt on
    /// "+ New playlist…".
    fn confirm_add_picker(&mut self) {
        let (pending, cursor) = match self.playlists.modal.take() {
            Some(PlaylistModal::AddPicker { pending, cursor }) => (pending, cursor),
            other => {
                self.playlists.modal = other;
                return;
            }
        };
        match self.playlists.store.playlists().get(cursor).map(|p| p.name.clone()) {
            Some(name) => self.apply_pending_add(&name, pending),
            None => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(pending),
                    input: String::new(),
                    error: None,
                });
            }
        }
    }

    /// Enter in the name prompt opened from "+ New playlist…": create the playlist, then add.
    fn confirm_create_and_add(&mut self) {
        let (pending, input) = match self.playlists.modal.take() {
            Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::CreateAndAdd(pending),
                input,
                ..
            }) => (pending, input),
            other => {
                self.playlists.modal = other;
                return;
            }
        };
        match self.playlists.store.create(&input) {
            Ok(name) => {
                // Tab 5's list cursor follows the new playlist to its sorted position.
                if let Some(index) = self.playlists.store.index_of(&name) {
                    self.playlists.list_cursor = index;
                    self.playlists.track_cursor = 0;
                }
                self.apply_pending_add(&name, pending);
            }
            // Invalid or duplicate name: keep the prompt open with the error.
            Err(e) => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(pending),
                    input,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    fn apply_pending_add(&mut self, playlist: &str, pending: PendingAdd) {
        match pending {
            PendingAdd::Entries { entries, .. } => self.add_entries_to_playlist(playlist, entries),
            PendingAdd::Folder { dir, .. } => self.start_folder_add(playlist, dir),
        }
    }

    /// Add `entries` to `playlist`, report the result and remember the playlist for the picker.
    fn add_entries_to_playlist(&mut self, playlist: &str, entries: Vec<PlaylistEntry>) {
        let single_title = match entries.as_slice() {
            [only] => Some(only.title.clone()),
            _ => None,
        };
        match self.playlists.store.add(playlist, entries) {
            Ok(crate::playlist_store::AddResult { added, skipped, .. }) => {
                let (stored, total) = self
                    .playlists
                    .store
                    .get(playlist)
                    .map(|p| (p.name.clone(), p.entries.len()))
                    .unwrap_or_else(|| (playlist.to_string(), 0));
                self.status_message = add_result_message(&stored, single_title.as_deref(), added, skipped, total);
                self.settings_mgr.settings.last_playlist = Some(stored);
                self.settings_mgr.save();
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }

    /// List `dir` on a worker (it may be on a slow or unmounted share);
    /// `poll_playlist_folder_adds` adds the files when the listing arrives.
    fn start_folder_add(&mut self, playlist: &str, dir: PathBuf) {
        self.status_message = format!("Reading {}…", folder_display_name(&dir));
        let tx = self.playlists.folder_tx.clone();
        let playlist = playlist.to_string();
        self.workers.spawn("playlist-folder", move || {
            let result = list_folder_audio_files(&dir);
            let _ = tx.send(FolderListing { playlist, dir, result });
        });
    }

    /// Apply finished folder listings from `start_folder_add`.
    fn poll_playlist_folder_adds(&mut self) {
        while let Ok(listing) = self.playlists.folder_rx.try_recv() {
            let folder = folder_display_name(&listing.dir);
            match listing.result {
                Ok(files) if files.is_empty() => {
                    self.status_message = format!("No audio files in {}", folder);
                }
                Ok(files) => {
                    let entries = files.iter().map(|path| self.playlist_entry_for_path(path)).collect();
                    self.add_entries_to_playlist(&listing.playlist, entries);
                }
                Err(e) => {
                    crate::log_warn!("playlists", "Couldn't read {}: {}", listing.dir.display(), e);
                    self.status_message = format!("Couldn't read {}", folder);
                }
            }
        }
    }

    /// The add picker: what is being added, one row per playlist, then "+ New playlist…".
    fn render_add_picker(&self, f: &mut Frame, pending: &PendingAdd, cursor: usize) {
        let area = super::centered_rect(60, 60, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " ADD TO PLAYLIST ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " [Enter] Add   [j/k] Move   [Esc] Cancel ",
                Style::default().fg(Color::DarkGray),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner);
        let header = Paragraph::new(Line::from(vec![
            Span::styled("Adding: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                pending.label().to_string(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]));
        f.render_widget(header, chunks[0]);

        let mut items: Vec<ListItem> = self
            .playlists
            .store
            .playlists()
            .iter()
            .map(|p| ListItem::new(format!("{}  ({})", p.name, track_count_label(p.entries.len()))))
            .collect();
        items.push(ListItem::new(Span::styled(
            NEW_PLAYLIST_ROW,
            Style::default().fg(Color::Green),
        )));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(list, chunks[1], &mut state);
    }
}
```

- [ ] **Step 12c: Wire the picker into the Task 4 and 5 modal, render and poll functions**

This step is the only wiring Task 6 does. It overrides Task 5's "Notes for Task 6":
- Task 6 handles AddPicker keys and Enter in a `CreateAndAdd` prompt before Task 5's handler runs, through `handle_add_picker_key` and `confirm_create_and_add` (Step 12b).
- Do not edit `submit_name_prompt`, and do not replace the `modal @ PlaylistModal::AddPicker { .. }` arm of `handle_playlist_modal_key`. Leave Task 5's `CreateAndAdd` and `AddPicker` arms as they are. The intercepts below return first, so those arms are never reached.
- Task 5's `render_playlist_modal` draws nothing for `AddPicker`, so this step adds the picker rendering.

In `AppUI::handle_playlist_modal_key` (`src/ui/playlists.rs`), find the Ctrl+C check at the top of the body:
```rust
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
        }
```
Insert the code below right after that check. It must come before any `NamePrompt` or `ConfirmDelete` handling and before the Task 4 Esc fallback:
```rust
        if matches!(self.playlists.modal, Some(PlaylistModal::AddPicker { .. })) {
            self.handle_add_picker_key(key);
            return Ok(false);
        }
        // Enter in a prompt opened from "+ New playlist…" creates the playlist and adds to it;
        // typing, Backspace and Esc stay with the shared name prompt handling below.
        if key.code == KeyCode::Enter
            && matches!(
                self.playlists.modal,
                Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(_),
                    ..
                })
            )
        {
            self.confirm_create_and_add();
            return Ok(false);
        }
```
At the top of the body of `AppUI::render_playlist_modal(&self, f: &mut Frame)`, insert:
```rust
        if let Some(PlaylistModal::AddPicker { pending, cursor }) = &self.playlists.modal {
            self.render_add_picker(f, pending, *cursor);
            return;
        }
```
At the end of the body of `AppUI::poll_playlists`, add:
```rust
        self.poll_playlist_folder_adds();
```

- [ ] **Step 12d: Bind `L` in the global hotkey match**

In `src/ui/mod.rs`, `handle_input_key`, replace:
```rust
                    crate::analysis::MixProfile::SeedRadio(_) => 0,
                };
            }
            _ => {
                // Tab specific key handling
```
with:
```rust
                    crate::analysis::MixProfile::SeedRadio(_) => 0,
                };
            }
            (KeyCode::Char('L'), _) | (KeyCode::Char('l'), KeyModifiers::SHIFT) => {
                self.open_add_picker();
            }
            _ => {
                // Tab specific key handling
```

- [ ] **Step 13: Run the tests and confirm that they pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
```
Expected: 10 passed.

- [ ] **Step 14: Write the failing help-modal test**

Append to `tests/test_playlists_add.rs`:
```rust
#[test]
fn test_help_modal_lists_add_to_playlist_key() {
    common::isolate();
    let dir = scratch_dir("help");
    let mut app = new_app(&dir);
    app.show_help_modal = true;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let view = terminal.backend().to_string();
    assert!(view.contains("Add to playlist"), "help modal should list L:\n{}", view);
    let _ = fs::remove_dir_all(&dir);
}
```

- [ ] **Step 15: Run the test and confirm that it fails**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add test_help_modal_lists_add_to_playlist_key -- --test-threads=1
```
Expected: FAILED with `help modal should list L:`.

- [ ] **Step 16: Add the help line**

In `src/ui/mod.rs`, `render_help_modal`, replace:
```rust
            Self::help_key_line("a", "Add all songs in folder"),
```
with:
```rust
            Self::help_key_line("a", "Add all songs in folder"),
            Self::help_key_line("L", "Add to playlist"),
```
No height change is needed. Task 4 sizes the help modal with `HELP_MODAL_HEIGHT = 30`, which leaves 27 content rows. The right column grows to 18 lines, so it still fits. Leave `help_modal_rect` alone, because it now sizes only the mix picker.

- [ ] **Step 17: Run the tests and confirm that they pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo test --test test_playlists_add -- --test-threads=1
cargo test --test test_browser test_help_modal_lifecycle_and_render -- --test-threads=1
```
Expected: 11 passed in `test_playlists_add`, and the existing help-modal render test still passes.

- [ ] **Step 18: Run the full suite, fmt and clippy**

```bash
export PATH="$HOME/.cargo/bin:$PATH"; cd /Users/jp/Development/quickstream
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```
Expected:
- `cargo fmt --check` prints nothing.
- clippy finishes with no warnings.
- Every test passes. This includes `tests/test_isolation.rs`, which checks that each new `#[test]` starts with `common::isolate()`, and the Task 4 and 5 tests in `tests/test_playlists_ui.rs`.

- [ ] **Step 19: Commit**

```bash
cd /Users/jp/Development/quickstream
git add src/settings.rs src/ui/mod.rs src/ui/playlists.rs tests/test_playlists_add.rs
git commit -m "$(cat <<'EOF'
feat: add key L and add-to-playlist picker (#37)

L adds the selected browser track or folder, the selected queue track,
or the now-playing track to a playlist chosen in a picker that starts on
the playlist last added to (Settings.last_playlist). A folder add lists
the folder's audio files on a worker, in name order and not recursive.
The picker's "+ New playlist…" row creates the playlist, then adds to it.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```


---

### Task 7: Existence checks, play and queue

**Files:**
- Modify: `src/ui/playlists.rs` (created in Task 4)
  - add the `ExistsChecker` type, the private `PlaylistChecks` and `QueueCheckResult` structs, and the free functions `mount_hint` and `missing_row_text`;
  - `pub struct PlaylistsUi`: add the field `checks: PlaylistChecks,` right after Task 4's line `    pub exists: HashMap<PathBuf, bool>,` (the doc comment above `exists` stays where it is). Task 6's folder fields may already follow `exists`. Either order works;
  - `PlaylistsUi::new`: add `checks: PlaylistChecks::new(),` right after the line that initialises `exists`;
  - add a new `impl PlaylistsUi` block with `set_exists_checker`, `is_missing`, `checks_in_flight` and `stop_checks`;
  - add a new `impl AppUI` block with `request_playlist_exists_checks`, `poll_playlist_checks`, `queue_selected_playlist`, `apply_playlist_queue` and `clear_queue_for_playlist`;
  - `pub fn poll_playlists(&mut self)` (Task 4, extended by Task 6): add `self.poll_playlist_checks();` as its last statement;
  - `fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers)` (Task 4): add two arms at the top of its `match (code, modifiers)`;
  - `fn render_playlists_tab(&mut self, f: &mut Frame, area: Rect)` (Task 4): in the tracks-pane row closure, replace the tail `ListItem::new(track_row_label(idx, entry)).style(style)` with a `let text = if ui.is_missing(…) { … } else { … };` followed by `ListItem::new(text).style(style)`.
- Modify: `src/ui/mod.rs`
  - `pub fn start_queue_metadata_fetch(&mut self)` (line 991): extend the candidate filter `.filter(|t| t.duration_seconds <= 0.0 || t.artist.is_empty())` with `|| t.sample_rate_hz == 0`;
  - `pub fn shutdown_workers(&mut self, timeout: Duration) -> usize`: after the loop `for stop in stops.into_iter().flatten() { … }` (line 1501), add `self.playlists.stop_checks();`;
  - `fn render_help_modal(&self, f: &mut Frame)`: add one `Self::help_key_line("Enter/a", "Play / queue playlist")` line after the last line of the tab 5 section that Tasks 4 and 5 added.
- Create (Test): `tests/test_playlists_play.rs`. This is its own test binary, so its helpers and imports cannot clash with the ones Tasks 4 to 6 put in `tests/test_playlists_ui.rs`. Task 8's module map can list it.

**Interfaces:**

Consumes:
- Task 1 (`src/playlist.rs`):
  - `PlaylistEntry { path, duration_secs, artist, title }` and `PlaylistEntry::display_label(&self) -> String`;
  - `Playlist { name, entries }` and `Playlist::to_m3u8(&self) -> String`;
  - `QueuePlan { tracks, start_index, missing }`;
  - `plan_queue(entries: &[PlaylistEntry], exists: &[bool], selected: Option<usize>, cached: &dyn Fn(&Path) -> Option<AudioMetadata>) -> QueuePlan`. Kept entries that are not cached get `placeholder_metadata`, which carries the `#EXTINF` artist and duration but `sample_rate_hz == 0`.
- Task 1 (`src/paths.rs`): `paths::get_playlists_dir() -> PathBuf`.
- Task 3 (`PlaylistStore`): `is_loaded()`, `playlists() -> &[Playlist]`, `get(&str)`, `index_of(&str)`, and `StoreError::NotLoaded` (its `Display` is "Playlists are still loading").
- Task 4:
  - `PlaylistsUi { store, list_cursor, track_cursor, tracks_focus, exists, tracks_state, .. }`, with `exists: HashMap<PathBuf, bool>`;
  - the free function `track_row_label(index: usize, entry: &PlaylistEntry) -> String`;
  - `AppUI::start_playlist_load()`, `AppUI::poll_playlists()`, `handle_playlists_tab_key`, and `render_playlists_tab` (which binds `let ui = &mut self.playlists;`);
  - the tab 5 section of the help modal (`HELP_MODAL_HEIGHT = 30`).
- Existing code in `src/ui/mod.rs`:
  - fields: `AppUI.workers` (`WorkerRegistry::spawn`), `metadata_cache` (`MetadataCache::get` / `insert`), `queue`, `queue_cursor`, `queue_metadata_attempted`, `player`, `web_server`, `autopilot_scan_stop`, `autopilot_rx`, `is_autopilot_scanning`, `autopilot_dir`, `status_message`;
  - methods: the private `enqueue_tracks(&mut self, Vec<AudioMetadata>) -> usize`, which calls `start_queue_metadata_fetch()`, plus `play_current_track(&mut self)` and `update_web_state(&mut self)`;
  - `WebServerHandle::close_active_streams()` and `stream_state().continuous.{current_track_index, current_position}`.

Produces (all in `src/ui/playlists.rs`. None of these names are in the contract, so they are declared here):
```rust
pub type ExistsChecker = std::sync::Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>;
pub fn mount_hint(path: &std::path::Path) -> String;                       // "/Volumes/Share/A/b.flac" -> "/Volumes/Share"; pure
pub fn missing_row_text(index: usize, entry: &crate::playlist::PlaylistEntry) -> String; // "<n>. ✗ missing: <label>  ----"
impl PlaylistsUi {
    #[doc(hidden)] pub fn set_exists_checker(&mut self, checker: ExistsChecker);
    pub fn is_missing(&self, path: &std::path::Path) -> bool;   // only true after a check found the path missing
    pub fn checks_in_flight(&self) -> usize;
    pub fn stop_checks(&self);
}
impl AppUI {
    pub fn request_playlist_exists_checks(&mut self);            // called on every poll; does nothing unless tab 5 is active
    fn poll_playlist_checks(&mut self);                          // called at the end of poll_playlists
    pub fn queue_selected_playlist(&mut self, play_from: Option<usize>); // Some(i) = Enter (replace + play), None = a (append)
    fn apply_playlist_queue(&mut self, result: QueueCheckResult);
    fn clear_queue_for_playlist(&mut self);
}
```
Changed behaviour in `src/ui/mod.rs`: `start_queue_metadata_fetch` also fetches tags for queue tracks with `sample_rate_hz == 0`. A playlist placeholder has an artist and a duration from `#EXTINF` but no album or audio properties, and the old filter skipped it. Spec Section 3 step 3 relies on this fetch. `queue_metadata_attempted` still limits each path to one read.

Key bindings on tab 5:
- `Enter` in the tracks pane calls `queue_selected_playlist(Some(track_cursor))`.
- `a` in either pane calls `queue_selected_playlist(None)`.
- `Enter` in the left pane still moves focus to the tracks pane (Task 4).

One addition to the contract: a replace also sets `autopilot_dir = None`. The web dashboard sets `state.is_autopilot = self.autopilot_dir.is_some()`, so this stops it from showing an autopilot badge for a playlist queue.

---

- [ ] **Step 1: Write failing tests for the helpers and the existence checks**

Create `tests/test_playlists_play.rs`:

```rust
//! Tab 5 playlists: missing-file checks, play (`Enter`) and queue (`a`) (issue #37).
//!
//! Existence checks, including the ones behind play/queue, run on workers because a `stat` on an
//! unmounted share can hang. Run with `--test-threads=1`: the tests share one playlists folder.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickstream::playlist::{Playlist, PlaylistEntry};
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{missing_row_text, mount_hint, ExistsChecker};
use quickstream::ui::AppUI;

const PLAYLIST: &str = "Road Trip";
const UI_BUDGET: Duration = Duration::from_millis(200);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_pl_play_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn touch(path: &Path) {
    std::fs::write(path, b"not really audio").unwrap();
}

fn entry(path: &Path, artist: &str, title: &str) -> PlaylistEntry {
    PlaylistEntry {
        path: path.to_path_buf(),
        duration_secs: 200,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Replace the playlists folder with a single `PLAYLIST` file and return its path.
fn write_playlist(entries: Vec<PlaylistEntry>) -> PathBuf {
    let dir = quickstream::paths::get_playlists_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let playlist = Playlist {
        name: PLAYLIST.to_string(),
        entries,
    };
    let file = dir.join(format!("{}.m3u8", PLAYLIST));
    std::fs::write(&file, playlist.to_m3u8()).unwrap();
    file
}

/// Call `poll_playlists` until `cond` holds or 2 seconds pass.
fn poll_until(app: &mut AppUI, cond: impl Fn(&AppUI) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        app.poll_playlists();
        if cond(app) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An app whose store has loaded `PLAYLIST`, selected in tab 5's left pane. The active tab is
/// left at its default (not tab 5), so no existence check has started yet.
fn app_with_playlist(dir: &Path) -> AppUI {
    let mut app = AppUI::new(
        Some(dir.to_path_buf()),
        SettingsManager::with_path(dir.join("config.json")),
    );
    app.start_playlist_load();
    assert!(
        poll_until(&mut app, |app| app.playlists.store.get(PLAYLIST).is_some()),
        "playlist did not load"
    );
    app.playlists.list_cursor = app.playlists.store.index_of(PLAYLIST).unwrap();
    app
}

fn render(app: &mut AppUI, width: u16, height: u16) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_mount_hint_is_the_first_two_folders() {
    common::isolate();
    assert_eq!(
        mount_hint(Path::new("/Volumes/Share/Radiohead/OK Computer/01 Airbag.flac")),
        "/Volumes/Share"
    );
    assert_eq!(mount_hint(Path::new("/Volumes/Share")), "/Volumes/Share");
    assert_eq!(mount_hint(Path::new("/song.flac")), "/song.flac");
}

#[test]
fn test_missing_row_text_marks_the_entry() {
    common::isolate();
    let ghost = entry(Path::new("/music/gone.flac"), "Ghost", "Gone");
    assert_eq!(missing_row_text(1, &ghost), "2. ✗ missing: Ghost - Gone  ----");
}

#[test]
fn test_missing_entries_render_as_missing_and_playlist_is_untouched() {
    common::isolate();
    let dir = temp_dir("missing_rows");
    let present = dir.join("present.mp3");
    let ghost = dir.join("ghost.mp3");
    touch(&present);
    let file = write_playlist(vec![entry(&present, "Alpha", "One"), entry(&ghost, "Ghost", "Gone")]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);

    app.active_tab = 4;
    assert!(
        poll_until(&mut app, |app| app.playlists.exists.len() == 2),
        "existence checks did not finish"
    );
    assert!(app.playlists.is_missing(&ghost));
    assert!(!app.playlists.is_missing(&present));

    let view = render(&mut app, 120, 30);
    assert!(view.contains("✗ missing: Ghost - Gone  ----"), "{}", view);
    assert!(view.contains("Alpha - One"), "{}", view);
    assert!(!view.contains("✗ missing: Alpha"), "{}", view);

    // A missing file never changes the playlist or its file.
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_hung_existence_check_does_not_block_ui() {
    common::isolate();
    let dir = temp_dir("hung_check");
    let first = dir.join("first.mp3");
    let ghost = dir.join("ghost.mp3");
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&ghost, "Ghost", "Gone")]);
    let mut app = app_with_playlist(&dir);

    // Each check blocks like a stat on a hung share until the gate's sender is dropped, then
    // reports the path missing. The watchdog drops it after 2s so a blocking UI fails the
    // timing asserts below instead of hanging the test.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Arc::new(Mutex::new(gate_rx));
    let checker: ExistsChecker = Arc::new(move |_path: &Path| {
        let _ = gate_rx.lock().unwrap().recv();
        false
    });
    app.playlists.set_exists_checker(checker);
    let (open_tx, open_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let _ = open_rx.recv_timeout(Duration::from_secs(2));
        drop(gate_tx);
    });

    app.active_tab = 4;
    let start = Instant::now();
    app.poll_playlists();
    let elapsed = start.elapsed();
    assert!(elapsed < UI_BUDGET, "poll_playlists blocked for {:?}", elapsed);
    assert_eq!(app.playlists.checks_in_flight(), 2);

    // Rows not checked yet render normally.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("Ghost - Gone"), "{}", view);
    assert!(!view.contains("✗ missing"), "{}", view);

    // Polling again neither blocks nor checks the same paths a second time.
    let start = Instant::now();
    app.poll_playlists();
    let elapsed = start.elapsed();
    assert!(elapsed < UI_BUDGET, "poll_playlists blocked for {:?}", elapsed);
    assert_eq!(app.playlists.checks_in_flight(), 2);

    open_tx.send(()).unwrap();
    watchdog.join().unwrap();
    assert!(
        poll_until(&mut app, |app| app.playlists.checks_in_flight() == 0),
        "checks did not finish after the gate opened"
    );
    assert!(app.playlists.is_missing(&first));
    assert!(app.playlists.is_missing(&ghost));

    // Results are cached for the session: showing the playlist again starts no new check.
    app.poll_playlists();
    assert_eq!(app.playlists.checks_in_flight(), 0);
    let view = render(&mut app, 120, 30);
    assert!(view.contains("✗ missing: Ghost - Gone"), "{}", view);
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlists_play -- --test-threads=1
```
Expected: the file does not compile. The errors are
`error[E0432]: unresolved imports `quickstream::ui::playlists::missing_row_text`, `quickstream::ui::playlists::mount_hint`, `quickstream::ui::playlists::ExistsChecker``
and `error[E0599]: no method named `set_exists_checker` / `is_missing` / `checks_in_flight` found for struct `PlaylistsUi``.

- [ ] **Step 3: Implement the existence-check worker, the session cache and the missing rows**

In `src/ui/playlists.rs`, add these module-level items after the `PendingAdd` enum:

```rust
/// Existence test used by the playlist workers. Tests swap in a slow one to stand in for a
/// hung network share.
pub type ExistsChecker = std::sync::Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>;

/// A playlist snapshot whose files were checked on a worker, ready to be queued.
struct QueueCheckResult {
    name: String,
    entries: Vec<crate::playlist::PlaylistEntry>,
    exists: Vec<bool>,
    /// Track to play from (`Enter`: replace the queue); `None` appends (`a`).
    play_from: Option<usize>,
}

/// Background existence checks for tab 5. Files are only checked on workers: a `stat` on an
/// unmounted share can hang, and the UI must keep drawing.
struct PlaylistChecks {
    checker: ExistsChecker,
    /// Paths sent to an existence-check worker whose result has not arrived yet.
    in_flight: std::collections::HashSet<std::path::PathBuf>,
    exists_tx: std::sync::mpsc::Sender<(std::path::PathBuf, bool)>,
    exists_rx: std::sync::mpsc::Receiver<(std::path::PathBuf, bool)>,
    queue_tx: std::sync::mpsc::Sender<QueueCheckResult>,
    queue_rx: std::sync::mpsc::Receiver<QueueCheckResult>,
    /// Set on shutdown: workers stop checking and send nothing more.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PlaylistChecks {
    fn new() -> Self {
        let (exists_tx, exists_rx) = std::sync::mpsc::channel();
        let (queue_tx, queue_rx) = std::sync::mpsc::channel();
        let checker: ExistsChecker = std::sync::Arc::new(|path: &std::path::Path| path.exists());
        Self {
            checker,
            in_flight: std::collections::HashSet::new(),
            exists_tx,
            exists_rx,
            queue_tx,
            queue_rx,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// The first two folders of `path` (`/Volumes/Music/...` -> `/Volumes/Music`): the share a
/// playlist whose files are all missing most likely lives on. Pure: no file system access.
pub fn mount_hint(path: &std::path::Path) -> String {
    let mut hint = std::path::PathBuf::new();
    let mut folders = 0;
    for component in path.components() {
        if matches!(component, std::path::Component::Normal(_)) {
            if folders == 2 {
                break;
            }
            folders += 1;
        }
        hint.push(component);
    }
    hint.display().to_string()
}

/// Row text for a playlist entry whose file was not found: `<n>. ✗ missing: <label>  ----`.
pub fn missing_row_text(index: usize, entry: &crate::playlist::PlaylistEntry) -> String {
    format!("{}. ✗ missing: {}  ----", index + 1, entry.display_label())
}
```

In `pub struct PlaylistsUi`, find Task 4's field line (its doc comment sits above it):

```rust
    pub exists: HashMap<PathBuf, bool>,
```

Insert directly after it:

```rust
    checks: PlaylistChecks,
```

In `PlaylistsUi::new`, insert this initialiser right after the line that initialises `exists` (`exists: HashMap::new(),`):

```rust
            checks: PlaylistChecks::new(),
```

Add a new `impl PlaylistsUi` block:

```rust
impl PlaylistsUi {
    /// Replace the existence test (tests use a blocking one to stand in for a hung share).
    #[doc(hidden)]
    pub fn set_exists_checker(&mut self, checker: ExistsChecker) {
        self.checks.checker = checker;
    }

    /// Whether a check this session found `path` missing. Paths not checked yet are not missing.
    pub fn is_missing(&self, path: &std::path::Path) -> bool {
        self.exists.get(path) == Some(&false)
    }

    /// Number of paths whose existence check is still running.
    pub fn checks_in_flight(&self) -> usize {
        self.checks.in_flight.len()
    }

    /// Tell playlist workers to stop checking (shutdown). A check already stuck in a hung `stat`
    /// is detached by the bounded worker join.
    pub fn stop_checks(&self) {
        self.checks.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
```

Add a new `impl AppUI` block:

```rust
impl AppUI {
    /// Check, on a worker, the entries of the playlist shown in the tracks pane that were not
    /// checked this session and are not being checked now. Called on every poll; never stats on
    /// the UI thread.
    pub fn request_playlist_exists_checks(&mut self) {
        if self.active_tab != 4 {
            return;
        }
        let ui = &mut self.playlists;
        let Some(playlist) = ui.store.playlists().get(ui.list_cursor) else {
            return;
        };
        let mut paths = Vec::new();
        for entry in &playlist.entries {
            if !ui.exists.contains_key(&entry.path) && ui.checks.in_flight.insert(entry.path.clone()) {
                paths.push(entry.path.clone());
            }
        }
        if paths.is_empty() {
            return;
        }
        let checker = ui.checks.checker.clone();
        let tx = ui.checks.exists_tx.clone();
        let stop = ui.checks.stop.clone();
        // Results are sent one by one, so rows turn into `✗ missing` as each check finishes.
        self.workers.spawn("playlist-exists", move || {
            for path in paths {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let exists = checker(path.as_path());
                if tx.send((path, exists)).is_err() {
                    break;
                }
            }
        });
    }

    /// Apply finished existence checks, then start checks for anything newly shown.
    fn poll_playlist_checks(&mut self) {
        while let Ok((path, exists)) = self.playlists.checks.exists_rx.try_recv() {
            self.playlists.checks.in_flight.remove(&path);
            self.playlists.exists.insert(path, exists);
        }
        self.request_playlist_exists_checks();
    }
}
```

In `pub fn poll_playlists(&mut self)`, add this as the last statement of the function body:

```rust
        self.poll_playlist_checks();
```

In `fn render_playlists_tab`, `let ui = &mut self.playlists;` stays in scope through the tracks pane, and `ui.tracks_state` is used after the rows are built. Read `is_missing` through `ui` and not through `self.playlists`. Inside the closure, `self.playlists` would be a second borrow of a value that is already mutably borrowed (E0502). Find the last line of Task 4's tracks-row closure:

```rust
                ListItem::new(track_row_label(idx, entry)).style(style)
```

Replace it with:

```rust
                let text = if ui.is_missing(&entry.path) {
                    missing_row_text(idx, entry)
                } else {
                    track_row_label(idx, entry)
                };
                ListItem::new(text).style(style)
```

In `src/ui/mod.rs`, `pub fn shutdown_workers`, find this existing anchor:

```rust
        for stop in stops.into_iter().flatten() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
```

Insert directly after it:

```rust
        self.playlists.stop_checks();
```

`queue_tx`, `queue_rx` and `QueueCheckResult` stay unused until Step 7, so a `cargo check` now shows `dead_code` warnings for them. Step 4 runs only the tests, and Step 9 runs clippy after Step 7 uses them.

- [ ] **Step 4: Run the tests and confirm they pass**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlists_play -- --test-threads=1
```
Expected: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 5: Write failing tests for play (`Enter`) and queue (`a`)**

In `tests/test_playlists_play.rs`, find this import:

```rust
use quickstream::playlist::{Playlist, PlaylistEntry};
```

Replace it with:

```rust
use quickstream::metadata::AudioMetadata;
use quickstream::player::AudioPlayer;
use quickstream::playlist::{Playlist, PlaylistEntry};
```

Then append to the end of the file:

```rust
/// Player stand-in that records commands instead of starting mpv.
struct RecordingPlayer {
    calls: Arc<Mutex<Vec<String>>>,
    playing: bool,
    paused: bool,
    volume: u8,
}

impl AudioPlayer for RecordingPlayer {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("play {}", filepath.display()));
        self.playing = true;
        self.paused = false;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.paused = true;
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.paused = false;
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        self.paused = !self.paused;
        Ok(self.paused)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("stop".to_string());
        self.playing = false;
        self.paused = false;
        Ok(())
    }
    fn seek(&mut self, _delta_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn seek_to(&mut self, _position_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume;
        Ok(())
    }
    fn volume(&self) -> u8 {
        self.volume
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn is_paused(&self) -> bool {
        self.paused
    }
    fn position_seconds(&self) -> f64 {
        0.0
    }
    fn duration_seconds(&self) -> f64 {
        0.0
    }
}

fn install_recording_player(app: &mut AppUI) -> Arc<Mutex<Vec<String>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    app.player = Box::new(RecordingPlayer {
        calls: calls.clone(),
        playing: false,
        paused: false,
        volume: 50,
    });
    calls
}

fn press(app: &mut AppUI, code: crossterm::event::KeyCode) {
    app.handle_input_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap();
}

fn queue_paths(app: &AppUI) -> Vec<PathBuf> {
    app.queue.tracks.iter().map(|t| t.filepath.clone()).collect()
}

fn old_track(dir: &Path) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join("old.mp3"),
        title: "Old".to_string(),
        ..Default::default()
    }
}

#[test]
fn test_enter_replaces_queue_stops_autopilot_and_plays_next_existing_track() {
    common::isolate();
    let dir = temp_dir("replace");
    let first = dir.join("a.mp3");
    let ghost = dir.join("ghost.mp3");
    let third = dir.join("c.mp3");
    touch(&first);
    touch(&third);
    let file = write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&ghost, "Ghost", "Gone"),
        entry(&third, "Gamma", "Three"),
    ]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    // Fully resolved tags: nothing left for the background fetch to fill in.
    app.metadata_cache.insert(AudioMetadata {
        filepath: first.clone(),
        title: "Cached One".to_string(),
        artist: "Alpha".to_string(),
        duration_seconds: 200.0,
        sample_rate_hz: 44_100,
        ..Default::default()
    });

    // An old queue and a running autopilot scan, both of which the replace must end.
    app.queue.add_track(old_track(&dir));
    let scan_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.autopilot_scan_stop = Some(scan_stop.clone());
    let (_autopilot_tx, autopilot_rx) = std::sync::mpsc::channel::<quickstream::library::AutopilotEvent>();
    app.autopilot_rx = Some(autopilot_rx);
    app.is_autopilot_scanning = true;
    app.autopilot_dir = Some(dir.clone());

    // Enter on the missing track plays the next existing one.
    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 1;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "Enter did not queue the playlist; status: {}",
        app.status_message
    );

    assert_eq!(app.status_message, "Queued 2 tracks from Road Trip, 1 missing");
    assert_eq!(queue_paths(&app), vec![first.clone(), third.clone()]);
    assert_eq!(app.queue.tracks[0].title, "Cached One", "cached metadata is used");
    assert_eq!(app.queue.current_index, 1);
    assert_eq!(app.queue_cursor, 1);

    // The uncached entry is a placeholder with an #EXTINF artist and duration but no audio
    // properties: its real tags (album, bitrate, sample rate) are still fetched in the background.
    assert_eq!(app.queue.tracks[1].artist, "Gamma");
    assert!(app.queue.tracks[1].duration_seconds > 0.0);
    assert!(
        app.queue_metadata_attempted.contains(&third),
        "placeholder with artist and duration was not sent for a tag fetch"
    );
    assert!(
        !app.queue_metadata_attempted.contains(&first),
        "fully cached track was fetched again"
    );

    let calls = calls.lock().unwrap().clone();
    let stop_at = calls.iter().position(|c| c == "stop").expect("playback was not stopped");
    let play_at = calls.iter().rposition(|c| c.starts_with("play ")).expect("playback did not start");
    assert!(stop_at < play_at, "calls: {:?}", calls);
    assert_eq!(calls[play_at], format!("play {}", third.display()));

    assert!(scan_stop.load(std::sync::atomic::Ordering::SeqCst), "autopilot scan not stopped");
    assert!(app.autopilot_rx.is_none());
    assert!(!app.is_autopilot_scanning);
    assert!(app.autopilot_dir.is_none());

    // The missing result is cached for the rows but never changes the playlist or its file.
    assert!(app.playlists.is_missing(&ghost));
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_a_appends_playlist_from_either_pane_without_touching_playback() {
    common::isolate();
    let dir = temp_dir("append");
    let first = dir.join("first.mp3");
    let second = dir.join("second.mp3");
    touch(&first);
    touch(&second);
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&second, "Beta", "Two")]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    app.queue.add_track(old_track(&dir));

    // Left pane.
    app.active_tab = 4;
    app.playlists.tracks_focus = false;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 3),
        "a did not append the playlist; status: {}",
        app.status_message
    );
    assert_eq!(app.status_message, "Queued 2 tracks from Road Trip");
    assert_eq!(queue_paths(&app), vec![dir.join("old.mp3"), first.clone(), second.clone()]);
    assert_eq!(app.queue.current_index, 0);

    // Tracks pane.
    app.playlists.tracks_focus = true;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 5),
        "a in the tracks pane did not append; status: {}",
        app.status_message
    );
    assert_eq!(app.queue.current_index, 0);
    assert!(calls.lock().unwrap().is_empty(), "append must not stop or start playback");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_all_missing_leaves_queue_unchanged() {
    common::isolate();
    let dir = temp_dir("all_missing");
    let file = write_playlist(vec![
        entry(Path::new("/nonexistent-qs-volume/Music/Album/1.flac"), "A", "One"),
        entry(Path::new("/nonexistent-qs-volume/Music/Album/2.flac"), "B", "Two"),
    ]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    app.queue.add_track(old_track(&dir));

    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("None of")),
        "no all-missing status; status: {}",
        app.status_message
    );

    assert_eq!(
        app.status_message,
        "None of Road Trip's 2 tracks found. Is /nonexistent-qs-volume/Music mounted?"
    );
    assert_eq!(queue_paths(&app), vec![dir.join("old.mp3")]);
    assert!(calls.lock().unwrap().is_empty(), "queue and playback must be left alone");
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_queueing_an_empty_playlist_reports_it() {
    common::isolate();
    let dir = temp_dir("empty");
    write_playlist(Vec::new());
    let mut app = app_with_playlist(&dir);
    app.queue.add_track(old_track(&dir));

    app.active_tab = 4;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert_eq!(app.status_message, "Road Trip is empty");
    assert_eq!(app.queue.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_help_modal_lists_playlist_play_and_queue_keys() {
    common::isolate();
    let dir = temp_dir("help");
    let mut app = AppUI::new(Some(dir.clone()), SettingsManager::with_path(dir.join("config.json")));
    app.show_help_modal = true;
    let view = render(&mut app, 120, 40);
    assert!(view.contains("Play / queue playlist"), "{}", view);
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 6: Run the tests and confirm the new ones fail**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlists_play -- --test-threads=1
```
Expected: the 4 Step 1 tests pass and 5 tests fail:
- `test_enter_replaces_queue_stops_autopilot_and_plays_next_existing_track`: panicked at `Enter did not queue the playlist; status: …`. Task 4's `Enter` only moves focus.
- `test_a_appends_playlist_from_either_pane_without_touching_playback`: panicked at `a did not append the playlist; status: …`.
- `test_all_missing_leaves_queue_unchanged`: panicked at `no all-missing status; status: …`.
- `test_queueing_an_empty_playlist_reports_it`: `assertion `left == right` failed`, where left is not `"Road Trip is empty"`.
- `test_help_modal_lists_playlist_play_and_queue_keys`: panicked with the rendered view, which has no `Play / queue playlist`.

- [ ] **Step 7: Implement play, queue, replace, the placeholder tag fetch and the help line**

In `src/ui/playlists.rs`, add these methods to the `impl AppUI` block from Step 3:

```rust
    /// `Enter` in the tracks pane (`play_from` = selected track) or `a` (`None`): check the
    /// selected playlist's files on a worker, then replace the queue and play, or append.
    pub fn queue_selected_playlist(&mut self, play_from: Option<usize>) {
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        let Some(playlist) = self.playlists.store.playlists().get(self.playlists.list_cursor) else {
            self.status_message = "No playlist selected".to_string();
            return;
        };
        if playlist.entries.is_empty() {
            self.status_message = format!("{} is empty", playlist.name);
            return;
        }
        let name = playlist.name.clone();
        let entries = playlist.entries.clone();
        self.status_message = format!("Checking {} tracks in {}…", entries.len(), name);
        let checker = self.playlists.checks.checker.clone();
        let tx = self.playlists.checks.queue_tx.clone();
        let stop = self.playlists.checks.stop.clone();
        self.workers.spawn("playlist-queue", move || {
            let mut exists = Vec::with_capacity(entries.len());
            for entry in &entries {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                exists.push(checker(entry.path.as_path()));
            }
            let _ = tx.send(QueueCheckResult {
                name,
                entries,
                exists,
                play_from,
            });
        });
    }

    /// Queue the files of a checked playlist that exist. Replace (`play_from` set) only clears
    /// the queue once something can be queued, so an all-missing playlist leaves it as it was.
    fn apply_playlist_queue(&mut self, result: QueueCheckResult) {
        // Fresh results update the session cache; they never change the playlist or its file.
        for (entry, exists) in result.entries.iter().zip(&result.exists) {
            self.playlists.exists.insert(entry.path.clone(), *exists);
        }
        let cached = |path: &std::path::Path| self.metadata_cache.get(path);
        let crate::playlist::QueuePlan {
            tracks,
            start_index,
            missing,
        } = crate::playlist::plan_queue(&result.entries, &result.exists, result.play_from, &cached);
        if tracks.is_empty() {
            let hint = result.entries.first().map(|e| mount_hint(&e.path)).unwrap_or_default();
            self.status_message = format!(
                "None of {}'s {} tracks found. Is {} mounted?",
                result.name,
                result.entries.len(),
                hint
            );
            return;
        }
        let queued = tracks.len();
        // enqueue_tracks starts the background tag fetch that fills in placeholder entries.
        match (result.play_from, start_index) {
            (Some(_), Some(start)) => {
                self.clear_queue_for_playlist();
                self.enqueue_tracks(tracks);
                self.queue.current_index = start;
                self.queue_cursor = start;
                self.play_current_track();
            }
            _ => {
                self.enqueue_tracks(tracks);
            }
        }
        // Set after play_current_track, which clears the status on success.
        self.status_message = if missing == 0 {
            format!("Queued {} tracks from {}", queued, result.name)
        } else {
            format!("Queued {} tracks from {}, {} missing", queued, result.name, missing)
        };
        self.update_web_state();
    }

    /// Before a playlist replaces the queue: stop any autopilot scan, end any continuous DLNA
    /// stream the way autopilot does before its own fresh start, stop playback, empty the queue.
    fn clear_queue_for_playlist(&mut self) {
        if let Some(stop) = self.autopilot_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.autopilot_rx = None;
        self.is_autopilot_scanning = false;
        // The queue is now the playlist, not an autopilot mix (web dashboard badge).
        self.autopilot_dir = None;
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
            *ws.stream_state().continuous.current_track_index.lock().unwrap() = 0;
            *ws.stream_state().continuous.current_position.lock().unwrap() = 0.0;
        }
        let _ = self.player.stop();
        self.queue.clear();
        self.queue_cursor = 0;
    }
```

In `fn poll_playlist_checks` (Step 3), find this existing anchor:

```rust
        while let Ok((path, exists)) = self.playlists.checks.exists_rx.try_recv() {
            self.playlists.checks.in_flight.remove(&path);
            self.playlists.exists.insert(path, exists);
        }
```

Insert directly after it:

```rust
        while let Ok(result) = self.playlists.checks.queue_rx.try_recv() {
            self.apply_playlist_queue(result);
        }
```

In `fn handle_playlists_tab_key`, insert these as the first arms of its `match (code, modifiers)`. They come before Task 4's `Enter` arm, so `Enter` in the left pane still moves focus to the tracks pane:

```rust
            (KeyCode::Enter, _) if self.playlists.tracks_focus => {
                self.queue_selected_playlist(Some(self.playlists.track_cursor));
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.queue_selected_playlist(None);
            }
```

In `src/ui/mod.rs`, `pub fn start_queue_metadata_fetch`, find this existing anchor:

```rust
    /// Resolve tags for queue tracks that still have placeholder metadata. Each path is read at
    /// most once per session by a persistent pool of 4 workers; results arrive via `metadata_rx`.
    pub fn start_queue_metadata_fetch(&mut self) {
        let candidates: Vec<PathBuf> = self
            .queue
            .tracks
            .iter()
            .filter(|t| t.duration_seconds <= 0.0 || t.artist.is_empty())
```

Replace it with:

```rust
    /// Resolve tags for queue tracks that still have placeholder metadata. Each path is read at
    /// most once per session by a persistent pool of 4 workers; results arrive via `metadata_rx`.
    /// A zero sample rate marks a placeholder that has an artist and duration (a playlist's
    /// `#EXTINF`) but no album or audio properties yet.
    pub fn start_queue_metadata_fetch(&mut self) {
        let candidates: Vec<PathBuf> = self
            .queue
            .tracks
            .iter()
            .filter(|t| t.duration_seconds <= 0.0 || t.artist.is_empty() || t.sample_rate_hz == 0)
```

The rest of the function stays as it is. The existing `.filter(|p| self.queue_metadata_attempted.insert(p.clone()))` still keeps each path to one read per session.

In `src/ui/mod.rs`, `fn render_help_modal`, add this line directly after the last `Self::help_key_line(...)` of the tab 5 (playlists) section that Tasks 4 and 5 added. Leave every existing tab 5 line in place, including Task 4's `Open playlist's tracks`:

```rust
            Self::help_key_line("Enter/a", "Play / queue playlist"),
```

- [ ] **Step 8: Run the tests and confirm they pass**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlists_play -- --test-threads=1
```
Expected: `test result: ok. 9 passed; 0 failed`.

Then run the earlier playlist UI tests and the full suite. Other tests must not regress, for example `test_worker_shutdown` with the new `stop_checks` call, and `test_queue_metadata_fetch` with the wider fetch filter:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo test --test test_playlists_ui -- --test-threads=1 && cargo test -- --test-threads=1
```
Expected: every test binary reports `test result: ok`.

- [ ] **Step 9: Format and lint**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH" && cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings
```
Expected: `cargo fmt --check` prints nothing, and clippy finishes with no warnings (`Finished` …).

- [ ] **Step 10: Commit**

```bash
git add src/ui/playlists.rs src/ui/mod.rs tests/test_playlists_play.rs
git commit -m "$(cat <<'EOF'
feat(playlists): play and queue playlists, show missing files

Tab 5 checks each shown playlist's files on a worker and caches the
results for the session; missing entries render as "✗ missing" with
"----". Enter in the tracks pane stops any autopilot scan and continuous
stream, replaces the queue with the playlist's existing files and plays
from the selected track (or the next existing one). a appends the whole
playlist. An all-missing playlist leaves the queue untouched and names
the share that is probably unmounted. Missing results never modify the
playlist or its file, and a hung check never blocks the UI.

Queued playlist placeholders carry an #EXTINF artist and duration but no
audio properties, so the queue tag fetch now also picks up tracks with a
zero sample rate (still at most one read per path per session).

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```


---

### Task 8: Docs: AGENTS.md and follow-up issue text

This task changes documentation only. No Rust code changes. The red/green cycle uses shell checks on `AGENTS.md` instead of Rust tests: each check fails before the edit and passes after it. A second check compares the documented key bindings with the real `match` arms. This task can run in parallel with Tasks 5–7. Run Step 6 (the key cross-check) after Tasks 5 and 6 have landed on `feat/playlists`.

**Files:**
- Modify: `/Users/jp/Development/quickstream/AGENTS.md`
  - §1.3 "Zero-Touch Library Invariant": the XDG list, anchor line `     - Bookmarks: \`~/.config/quickstream/bookmarks.json\`` (line 23).
  - §2 module map: anchors `│   ├── library.rs        #` (line 79), `│   │   └── search.rs     #` (line 64), `    ├── test_now_playing.rs   #` (line 95).
  - §3: the heading `### 4-Tab Layout Structure` and the Header bullet (lines 105–106), the Tab 3 bullet (line 113), the Mix Profile Modal and modal-dismissal bullets (lines 120–121), and the block from `### Key Navigation Invariants` (line 123) to `   - \`r\`: Start Seed Radio station from selected track.` (line 137).
- Test: none added (docs only). Verification uses `grep -F` checks on `AGENTS.md` plus `grep` on `src/ui/mod.rs` and `src/ui/playlists.rs`.
- Create (scratchpad, not committed): `/private/tmp/claude-501/-Users-jp-Development-quickstream/33d25c15-b110-4b2d-8f6d-768e3b917974/scratchpad/issue-web-playlists.md`, the body of the web playlists follow-up issue (Step 9), which Step 10 files on `jasonmarkperez/quickstream` with `gh`.

Facts checked against the current code (the global hotkey `match` in `AppUI::handle_input_key`, `src/ui/mod.rs` lines 2044–2296):
- Global arms: `q`/`Esc`/`Ctrl+C` (quit; while a directory scan is running, `Esc` cancels the scan instead), `?`, `Tab`, `BackTab`, `1`–`4`, `n`/`N`, `Space`, `s`, `Left`/`Right`, `Shift|Ctrl|Alt+Left/Right`, `[`/`]`, `<`/`>`, `+`/`=`, `-`, `v`, `/`, `f`, `z`/`Z`, `p`, `P` (also `Shift+p`), `M` (also `Shift+m`).
- No arm exists for `A`, `R`, `t`, or a plain `m`. Autopilot is `p`, not `A`. `r` is tab-specific: Seed Radio on tab 0, Seed Radio or directory refresh on tab 1, device rescan on tab 2.
- `seek_step_seconds` defaults to `10.0` in `impl Default for Settings` (`src/settings.rs` line 53 today; line 56 once Task 6 adds three lines to `struct Settings`: doc comment, `#[serde(default)]`, `last_playlist`).
- `J`/`K` on tab 1 are Page Down/Up. On tab 5 (tracks pane) they move the selected track (Task 5).
- Test files by task: Task 1 `tests/test_playlists.rs`, Task 2 `tests/test_persistence_worker.rs` (writer delete/rename), Task 3 `tests/test_playlist_store.rs`, Task 4 `tests/test_playlists_ui.rs`, Task 5 `tests/test_playlists_edit.rs`, Task 6 `tests/test_playlists_add.rs`, Task 7 `tests/test_playlists_play.rs`.

**Interfaces:**
- Consumes: names documented from the contract, with no code dependency. `crate::playlist::{PLAYLIST_EXT, playlist_file_path, validate_name}`, `crate::playlist_store::PlaylistStore`, `crate::persist::PersistHandle::{save, delete, rename}`, `crate::paths::get_playlists_dir() -> PathBuf`, `crate::ui::playlists::PlaylistsUi`, `AppUI::playlists: playlists::PlaylistsUi`, `Settings.last_playlist: Option<String>`, the global arm `(KeyCode::Char('L'), _)` calling `AppUI::open_add_picker()`.
- Produces: no Rust items. Produces updated `AGENTS.md` sections §1.3, §2 and §3, plus the filed follow-up issue for web playlists.

- [ ] **Step 1: Write the failing docs check**

Run it from any directory. It prints one line for every missing or stale passage:

```bash
cd /Users/jp/Development/quickstream
for p in \
  'Playlists: `~/.config/quickstream/playlists/<name>.m3u8`' \
  '├── playlist.rs       # Pure M3U8' 'playlist_store.rs' 'playlists.rs  # Tab 5' \
  'test_playlists.rs' 'test_playlist_store.rs' \
  'test_playlists_ui.rs  # Tab 5 tab switching' 'test_playlists_edit.rs # Name prompt' \
  'test_playlists_add.rs  # L key' 'test_playlists_play.rs # Missing-file checks' \
  'tests/test_persistence_worker.rs' \
  '### 5-Tab Layout Structure' '`[5] Playlists`' '**Tab 4 (`[5] Playlists`)**' \
  '**Add-to-Playlist Picker (`L`)**' '+ New playlist…' '**Playlist Name Prompt**' \
  '`L`: Add to playlist' 'There is no global `A`, `R`, `m` or `t` binding'; do
  grep -qF -- "$p" AGENTS.md || echo "MISSING: $p"
done
for p in '### 4-Tab Layout Structure' '`A`: Start / Restart Autopilot' '(`Space`, `s`, `R`, `m`, `t`,'; do
  grep -qF -- "$p" AGENTS.md && echo "STALE: $p"
done
true
```

- [ ] **Step 2: Run it and confirm it fails**

Run the Step 1 command.
Expected output (red): 19 `MISSING:` lines (one for every pattern in the first loop), then:
```
STALE: ### 4-Tab Layout Structure
STALE: `A`: Start / Restart Autopilot
STALE: (`Space`, `s`, `R`, `m`, `t`,
```

- [ ] **Step 3: Update §1.3 (persistent data list)**

Edit `/Users/jp/Development/quickstream/AGENTS.md`. Replace:

```markdown
     - Configuration: `~/.config/quickstream/config.json`
     - Bookmarks: `~/.config/quickstream/bookmarks.json`
```

with:

```markdown
     - Configuration: `~/.config/quickstream/config.json` (includes `last_playlist`, the playlist the add picker preselects)
     - Bookmarks: `~/.config/quickstream/bookmarks.json`
     - Playlists: `~/.config/quickstream/playlists/<name>.m3u8` (`paths::get_playlists_dir()`; extended M3U8, UTF-8, LF, absolute track paths, one file per playlist). Every write, rename and delete goes through `PlaylistStore` and `playlist::playlist_file_path`, which refuses any path whose parent is not the playlists folder. Missing-file checks never prune a playlist, and a file that fails to load is skipped and never rewritten.
```

- [ ] **Step 4: Update the §2 module map**

Edit 1. Replace:

```
│   │   ├── mod.rs        # AppUI state machine, 60 FPS render passes, tab switching, hotkey router
│   │   └── search.rs     # SearchModalState, streaming fuzzy search (fuzzy-matcher / SkimMatcherV2)
```

with:

```
│   │   ├── mod.rs        # AppUI state machine, 60 FPS render passes, tab switching, hotkey router
│   │   ├── playlists.rs  # Tab 5 Playlists: PlaylistsUi, name prompt / delete confirm / add picker, L key, play & queue
│   │   └── search.rs     # SearchModalState, streaming fuzzy search (fuzzy-matcher / SkimMatcherV2)
```

Edit 2. Replace:

```
│   ├── library.rs        # Recursive walkdir scanner, progressive autopilot streamer, JSONL cache
│   ├── metadata.rs       # Lofty audio metadata extraction, cover art decoding, path heuristics
```

with:

```
│   ├── library.rs        # Recursive walkdir scanner, progressive autopilot streamer, JSONL cache
│   ├── metadata.rs       # Lofty audio metadata extraction, cover art decoding, path heuristics
│   ├── playlist.rs       # Pure M3U8 playlist model: parse/write, name validation, safe file paths, queue plan
│   ├── playlist_store.rs # In-memory PlaylistStore (sorted), write-through to disk via PersistHandle
│   ├── persist.rs        # Background atomic writer: ordered save / delete / rename (PersistHandle)
```

Edit 3. Replace:

```
    ├── test_now_playing.rs   # Tab 0 Now Playing hero view layout and rendering
```

with:

```
    ├── test_now_playing.rs   # Tab 0 Now Playing hero view layout and rendering
    ├── test_playlists.rs     # M3U8 parse/write round trips, name rules, safe paths, queue planning
    ├── test_playlist_store.rs # Store create/add/remove/move/rename/delete, case-only rename, load skips
    ├── test_playlists_ui.rs  # Tab 5 tab switching, two-pane navigation, rendering, modal hook, help
    ├── test_playlists_edit.rs # Name prompt, create/rename/delete, K/J move, d/x remove
    ├── test_playlists_add.rs  # L key, add picker, folder adds, last_playlist
    ├── test_playlists_play.rs # Missing-file checks, Enter replace+play, a append, all-missing
```

Edit 4. Directly after the closing ```` ``` ```` of the module map code block (the line after `    └── test_web.rs           # Embedded web server, byte ranges, REST API, and command dispatch`), insert this paragraph, preceded by a blank line:

```markdown
The background writer's delete and rename tests (used by `PlaylistStore`) are in the existing `tests/test_persistence_worker.rs`, which the map above does not list.
```

- [ ] **Step 5: Update §3 (layout, modals, key invariants)**

Edit 1. Replace these two lines (105–106):

```markdown
### 4-Tab Layout Structure
- **Header**: Tab bar (`[1] Now Playing` | `[2] Queue & Library` | `[3] Devices` | `[4] Settings`).
```

with:

```markdown
### 5-Tab Layout Structure
- **Header**: Tab bar (`[1] Now Playing` | `[2] Queue & Library` | `[3] Devices` | `[4] Settings` | `[5] Playlists`).
```

Edit 2. Replace:

```markdown
  - **Tab 3 (`[4] Settings`)**: User preferences editor (metadata display, audio backend, seek interval, volume step, queue mode, PCM transcode, web server toggle, startup home directory).
```

with:

```markdown
  - **Tab 3 (`[4] Settings`)**: User preferences editor (metadata display, audio backend, seek interval, volume step, queue mode, PCM transcode, web server toggle, startup home directory).
  - **Tab 4 (`[5] Playlists`)**: Split-pane layout (`src/ui/playlists.rs`, state in `AppUI::playlists: PlaylistsUi`). A startup worker loads the playlists. Until the load finishes, the tab shows `Loading playlists…` and edits are refused with `Playlists are still loading`. Focus toggles with `h` and `l`. `j`/`k`/`Up`/`Down` move with wraparound.
    - **Left Pane (playlists)**: one row per playlist (name and track count), sorted case-insensitively. `c` create, `R` rename (prompt prefilled with the current name), `D` delete after `Delete "<name>" (<n> tracks)? y/n`, `Enter` focus the tracks pane, `a` append the whole playlist to the queue. Empty state: `No playlists yet. Press c to create one, or L on any track.`
    - **Right Pane (tracks)**: title `<NAME> (<n> tracks, <total length>)`, with unknown durations left out of the total. Rows read `<n>. <artist> - <title>  <m:ss>`. Missing files show `✗ missing: <artist> - <title>` and `----`. Existence is checked on a worker and cached per path for the session. Rows not yet checked render normally. `Enter` replaces the queue with the playlist and plays from the selected track: it stops any autopilot scan and continuous stream first and skips missing files. If every file is missing, it leaves the queue untouched. `a` appends the whole playlist. `K`/`J` move the selected track up/down (no wraparound; on tab `[2]` these keys page instead). `d`/`x`/`Del` remove it. Empty state: `Empty. Press L on a track in tab 2 to add it.`
    - Edits save automatically through the background writer (`PersistHandle`). There is no save key.
```

Edit 3. Replace:

```markdown
  - **Mix Profile Modal (`M`)**: Interactive autopilot mix profile selector (`[1]` Diverse Shuffle, `[2]` BPM Flow, `[3]` Chill & Ambient, `[4]` Workout, `[5]` Late Night Groove).
  - Modals intercept keyboard input when active; dismissed with `Esc` or `q`.
```

with:

```markdown
  - **Mix Profile Modal (`M`)**: Interactive autopilot mix profile selector (`[1]` Diverse Shuffle, `[2]` BPM Flow, `[3]` Chill & Ambient, `[4]` Workout, `[5]` Late Night Groove).
  - **Add-to-Playlist Picker (`L`)**: the header names what is being added (the track title, or `<folder> (folder)`). The rows list the playlists, with `last_playlist` highlighted (or the first row if it no longer exists), then `+ New playlist…`. `j`/`k`/arrows wrap, `Enter` adds, `Esc`/`q` cancel. Results: `Added "<title>" to <name> (<n> tracks)`, `Already in <name>`, `Added <k> to <name>, skipped <m> already in it`.
  - **Playlist Name Prompt** (`c`, `R`, `+ New playlist…`): every character is text, including `q` and digits. `Backspace` deletes, `Enter` confirms, `Esc` cancels. An invalid or duplicate name (case-insensitive) keeps the prompt open and shows the error, e.g. `Name can't contain /` or `"Road Trip" already exists`.
  - **Playlist Delete Confirmation (`D`)**: `y` deletes the file. Any other key cancels.
  - Modals intercept keyboard input when active, before the global hotkeys. Most are dismissed with `Esc` or `q`. The playlist name prompt and the Startup Home Directory editor treat `q` as text and close only with `Esc`/`Enter`. `Ctrl+C` quits from any modal.
```

Edit 4. Replace lines 123–137 exactly:

```markdown
### Key Navigation Invariants
1. **List Wraparound**:
   All interactive lists (Queue, FileBrowser, Devices, Search Results, Mix Profiles) wrap around: navigating `Up` at index `0` moves to the last item, and navigating `Down` at the end loops back to `0`.
2. **Keybinding Precedence**:
   - Terminal resize events handled first.
   - Active overlay modal input handled next (dismissal on `Esc`/`q`).
   - Global playback hotkeys (`Space`, `s`, `R`, `m`, `t`, `+`, `-`, arrows, Shift+arrows) work across all tabs.
   - Tab-specific and pane-specific keys handle item manipulation.
3. **Key Mappings**:
   - `Left` / `Right`: Global Previous / Next track.
   - `Shift+Left` / `Shift+Right`: Global Seek backward / forward by configured interval (default 10s).
   - `A`: Start / Restart Autopilot mode on current directory.
   - `M`: Open Mix Profile modal.
   - `P`: Reshuffle Autopilot seed from next track forward.
   - `r`: Start Seed Radio station from selected track.
```

with:

```markdown
### Key Navigation Invariants
1. **List Wraparound**:
   All interactive lists (Queue, FileBrowser, Devices, Search Results, Mix Profiles, Playlists, Playlist Tracks, Add-to-Playlist Picker) wrap around: navigating `Up` at index `0` moves to the last item, and navigating `Down` at the end loops back to `0`. Moving a playlist track with `K`/`J` does not wrap.
2. **Keybinding Precedence**:
   - Terminal resize events handled first.
   - Active overlay modal input handled next (see Overlay Modals for dismissal keys).
   - Global hotkeys in `AppUI::handle_input_key` come next, so tab handlers cannot reuse them: `q`/`Esc`/`Ctrl+C` (quit; while a directory scan is running, `Esc` cancels the scan instead), `?`, `Tab`/`Shift+Tab`, `1`–`5`, `n`, `Space`, `s`, `Left`/`Right`, the seek keys, `+`/`=`/`-`, `v`, `/`, `f`, `z`/`Z`, `p`, `P`, `M`, `L`. Some act only on certain tabs (`v`, `/`, `f`). There is no global `A`, `R`, `m` or `t` binding. `R` is the tab 5 rename key.
   - Tab-specific and pane-specific keys handle item manipulation.
3. **Key Mappings**:
   - `1`–`5` / `Tab` / `Shift+Tab`: Switch tab (cycling covers all 5 tabs). `n`: Toggle Now Playing ↔ Queue & Library.
   - `Space`: Play / Pause (on `[4] Settings` it cycles the selected value). `s`: Stop.
   - `Left` / `Right`: Global Previous / Next track (on `[4] Settings` they cycle the selected value).
   - `Shift`/`Ctrl`/`Alt` + `Left` / `Right`, `[` / `]`, `<` / `>`: Global Seek backward / forward by `seek_step_seconds` (default 10s).
   - `+` (or `=`) / `-`: Volume up / down by `volume_step`.
   - `p`: Start Autopilot on the current directory (or the highlighted Music Home).
   - `P`: Reshuffle Autopilot seed from next track forward.
   - `M`: Open Mix Profile modal.
   - `z` / `Z`: Shuffle the queue.
   - `r` (tab-specific): Seed Radio from the now-playing track on `[1]`; from the selected browser track or queue track on `[2]` (on a folder row it rescans the directory); rescans renderers on `[3]`.
   - `L`: Add to playlist (opens the Add-to-Playlist Picker). On `[2]` it adds the focused pane's selection. Browser track row: that track. Browser folder row: the audio files directly inside the folder, in name order, not recursive, listed on a worker (`Couldn't read <folder>` on error). `★ Music Homes` or `..` row: status `Select a track or folder`. Queue pane: the selected queue track. On every other tab it adds the now-playing track (`Nothing is playing` when idle). Entries are built from metadata already in memory (queue row, now playing, `MetadataCache`, else the file stem). The share is never read on the UI thread.
```

- [ ] **Step 6: Run the docs check and the key cross-check (expect PASS)**

Run the Step 1 command again.
Expected output (green): nothing.

Then compare the documented keys with the code (run this after Tasks 5 and 6 have landed):

```bash
cd /Users/jp/Development/quickstream
echo '--- A / t / plain m must not be bound:'; grep -nE "KeyCode::Char\('(A|t)'\)|KeyCode::Char\('m'\), KeyModifiers::NONE" src/ui/mod.rs src/ui/playlists.rs
echo '--- R only in the playlists module:'; grep -n "KeyCode::Char('R')" src/ui/mod.rs src/ui/playlists.rs
echo '--- global L arm:'; grep -n "(KeyCode::Char('L'), _)" src/ui/mod.rs
echo '--- tab cycling covers 5 tabs:'; grep -n "% 5" src/ui/mod.rs
echo '--- seek default:'; grep -n "seek_step_seconds: 10.0" src/settings.rs
echo '--- every mapped test file exists:'; ls tests/test_playlists.rs tests/test_playlist_store.rs tests/test_playlists_ui.rs tests/test_playlists_edit.rs tests/test_playlists_add.rs tests/test_playlists_play.rs tests/test_persistence_worker.rs
```

Expected:
- The first group prints no hits.
- The `R` group shows matches only under `src/ui/playlists.rs`.
- The `L` group shows exactly one hit in `src/ui/mod.rs`.
- The `% 5` group shows the `Tab` and `BackTab` arms.
- The seek group shows exactly one hit, in `src/settings.rs` (the `impl Default for Settings` line, `src/settings.rs:56` after Task 6). The line number itself is not checked.
- The `ls` lists all seven test files with no `No such file or directory` error. Run this group after Task 7 as well.

If any line differs, fix `AGENTS.md` to match the code. Do not change the code to match the docs.

- [ ] **Step 7: Run fmt, clippy and the isolation test**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd /Users/jp/Development/quickstream
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --test test_isolation -- --test-threads=1
```

Expected: `cargo fmt --check` prints nothing, clippy finishes with no warnings, and `test_isolation` passes. Only Markdown changed, so these checks confirm the tree is still clean.

- [ ] **Step 8: Commit**

```bash
cd /Users/jp/Development/quickstream
git add AGENTS.md
git commit -m "$(cat <<'EOF'
docs: AGENTS.md playlists tab, files and corrected key list

Document the playlists folder in the XDG data list, the new
playlist / playlist_store / ui::playlists modules and their tests,
the [5] Playlists tab, its modals and the L add key. Replace the
global key list with what handle_input_key actually binds (no A,
R, m or t; autopilot is p).

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 9: Write the follow-up issue body**

The spec asks for a follow-up issue for web playlists. Use the Write tool (not a shell heredoc) to create `/private/tmp/claude-501/-Users-jp-Development-quickstream/33d25c15-b110-4b2d-8f6d-768e3b917974/scratchpad/issue-web-playlists.md` with exactly this content (the text between the fences, without the fences):

```markdown
Follow-up to #37. The playlists feature shipped TUI-only in v1 (tab `[5] Playlists`, `L` to add). Playlists are stored as extended M3U8 files in `~/.config/quickstream/playlists/<name>.m3u8` and held in memory by `PlaylistStore` (`src/playlist_store.rs`). The web dashboard cannot see or use them yet.

## Goal
Let the web dashboard list playlists, show a playlist's tracks, play one (replace the queue and start from a track) or append it to the queue. As a second step, let it edit playlists (create, rename, delete, add, remove, reorder).

## Suggested shape
- Read: a `GET` endpoint next to `/api/status` / `/api/browser` in `src/web.rs` that returns playlist names, track counts and entries (label, duration, path). Serve it from a snapshot the UI thread publishes, never by reading the playlists folder per request.
- Play / append: new `WebCommand` variants dispatched through `/api/action` and applied on the UI thread with the same code tab 5 uses (`plan_queue`, stop autopilot scan and continuous stream on replace, `Queued <k> tracks from <name>, <m> missing`). The web queue path currently reports `(web)` in its status message. Decide whether playlist actions name the playlist instead.
- Edit: mutations go through `PlaylistStore` only, so every write, rename and delete stays behind `playlist::playlist_file_path` (playlists folder only) and the ordered background writer.

## Constraints
- All new endpoints keep the `web_access` checks (pairing token, Host/Origin/Content-Type).
- Playlist entry paths outside the allowed web folders must be rejected the same way `AddFile`/`AddFolder` are (`WebCommand::PathRejected`), even though the playlist file itself is trusted.
- Names from the web go through `playlist::validate_name` (same rules and messages as the TUI prompt, case-insensitive duplicates).
- Non-blocking UI invariant: existence checks and any disk reads run on workers.
- Zero-touch library: nothing writes outside `~/.config/quickstream/playlists/`.

## Out of scope
Importing `.m3u`/`.pls`/`.xspf` files from the share, rewriting path prefixes for a different mount point, and watching the playlists folder for outside edits. All three are still open from #37.
```

Check it:

```bash
F=/private/tmp/claude-501/-Users-jp-Development-quickstream/33d25c15-b110-4b2d-8f6d-768e3b917974/scratchpad/issue-web-playlists.md
head -n 1 "$F"; grep -c '^## ' "$F"
```

Expected: the first line starts with `Follow-up to #37.`, and the count is `4` (`## Goal`, `## Suggested shape`, `## Constraints`, `## Out of scope`).

- [ ] **Step 10: File the follow-up issue**

Title: `Web dashboard: playlists (list, play, append, edit)`
Labels: `enhancement`, `P2-medium`, `area:web` (all three already exist on `jasonmarkperez/quickstream`).

First make sure it has not been filed already (for example by an earlier run of this task):

```bash
gh issue list --repo jasonmarkperez/quickstream --state all \
  --search 'in:title "Web dashboard: playlists"' --json number,title,state
```

Expected: `[]`. If it lists an issue with this exact title, do not create a second one: skip the next command and report that issue's URL instead.

Then file it and confirm the title and labels in the same shell:

```bash
URL=$(gh issue create --repo jasonmarkperez/quickstream \
  --title "Web dashboard: playlists (list, play, append, edit)" \
  --label enhancement --label P2-medium --label area:web \
  --body-file /private/tmp/claude-501/-Users-jp-Development-quickstream/33d25c15-b110-4b2d-8f6d-768e3b917974/scratchpad/issue-web-playlists.md)
echo "$URL"
gh issue view "$URL" --json title,labels --jq '.title, ([.labels[].name] | sort | join(","))'
```

Expected: the new issue URL (`https://github.com/jasonmarkperez/quickstream/issues/` followed by its number), then `Web dashboard: playlists (list, play, append, edit)`, then `P2-medium,area:web,enhancement`. Report the issue URL to the coordinator so the PR description can link it. Nothing in the repository changes in Steps 9 and 10, so there is no second commit.


---

