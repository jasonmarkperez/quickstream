//! Issue #37: `PlaylistStore` keeps the playlists in memory, sorted by name, and writes every
//! edit through the background writer to `<playlists dir>/<name>.m3u8`.
//!
//! Every test uses its own temp folder as the playlists dir and a real `PersistenceWorker`;
//! nothing here touches the user's real config.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use quickstream::persist::PersistenceWorker;
use quickstream::playlist::{NameError, Playlist, PlaylistEntry};
use quickstream::playlist_store::{AddResult, MoveDir, PlaylistStore, StoreError};

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
    assert_eq!(
        store.create("ROAD TRIP").unwrap_err().to_string(),
        "\"Road Trip\" already exists"
    );
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
    assert_eq!(
        store.create(" LATIN ").unwrap_err().to_string(),
        "\"Latin\" already exists"
    );
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
        Ok(AddResult {
            added: 2,
            skipped: 1,
            total: 2
        })
    );
    assert_eq!(
        store.add("Road Trip", vec![entry("b"), entry("c")]),
        Ok(AddResult {
            added: 1,
            skipped: 1,
            total: 3
        })
    );
    assert_eq!(
        store.add("Road Trip", vec![entry("c")]),
        Ok(AddResult {
            added: 0,
            skipped: 1,
            total: 3
        })
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
    assert_eq!(
        store.move_entry("Mix", 3, MoveDir::Up),
        Err(StoreError::IndexOutOfRange)
    );
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
    assert_eq!(
        store.rename("Missing", "C"),
        Err(StoreError::NotFound("Missing".to_string()))
    );
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
    store
        .add("road trip", vec![entry("a"), entry("b"), entry("c")])
        .unwrap();
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
    assert_eq!(
        store.move_entry("../escape", 0, MoveDir::Down),
        Err(StoreError::UnsafePath)
    );
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

#[test]
fn test_rename_to_the_unchanged_name_is_ok_even_when_that_name_is_reserved() {
    common::isolate();
    let dir = scratch("rename_unchanged_reserved");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    assert!(worker.flush());
    // A skipped file with the same bytes (a case-sensitive disk) must not turn a no-op into an error.
    store.reserve_names(vec!["Mix".to_string()]);

    assert_eq!(store.rename("Mix", "Mix"), Ok("Mix".to_string()));
    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["Mix.m3u8"]);
}
