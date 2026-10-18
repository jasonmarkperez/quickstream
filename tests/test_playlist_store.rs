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
    let (playlists, skipped) = PlaylistStore::load_dir_reserving(dir).unwrap();
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
        .unwrap()
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
    assert!(PlaylistStore::load_dir(&dir).unwrap().is_empty());
    assert_eq!(
        PlaylistStore::load_dir_reserving(&dir).unwrap(),
        (Vec::new(), Vec::new())
    );
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

    let (loaded, skipped) = PlaylistStore::load_dir_reserving(&dir).unwrap();
    let loaded_names: Vec<&str> = loaded.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(loaded_names, vec!["Good"]);
    assert_eq!(skipped, vec!["Latin", "Mix", "folder"]);
    assert_eq!(PlaylistStore::load_dir(&dir).unwrap(), loaded);
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

    let loaded = PlaylistStore::load_dir(&dir).unwrap();
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
        Err(StoreError::Name(NameError::Reserved("Latin".to_string())))
    );
    assert_eq!(
        store.create(" LATIN ").unwrap_err().to_string(),
        "A playlist file named \"Latin\" exists but couldn't be loaded"
    );
    assert_eq!(
        store.create("mix"),
        Err(StoreError::Name(NameError::Reserved("Mix".to_string())))
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

/// `quickstream .` makes browser paths relative. Stored verbatim, a reload would resolve them
/// against the playlists folder and every track would be missing.
#[test]
fn test_add_stores_relative_paths_as_absolute() {
    common::isolate();
    let dir = scratch("add_relative");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    let mut rel = entry("a");
    rel.path = PathBuf::from("rel/a.mp3");
    store.add("Mix", vec![rel]).unwrap();
    assert!(worker.flush());

    let reloaded = on_disk(&dir, "Mix");
    assert_eq!(reloaded.entries.len(), 1);
    let path = &reloaded.entries[0].path;
    assert!(path.is_absolute(), "stored path {:?} is not absolute", path);
    assert!(path.ends_with("rel/a.mp3"), "stored path {:?}", path);
    assert_eq!(path, &std::env::current_dir().unwrap().join("rel/a.mp3"));
}

/// Issue #58: `quickstream ./music/../music` hands the store `..` paths. They are normalized
/// before the duplicate check, so the same file reached another way is not added twice and no
/// `..` is written to the file.
#[test]
fn test_add_normalizes_paths_before_the_duplicate_check() {
    common::isolate();
    let dir = scratch("add_normalize");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    let mut plain = entry("x");
    plain.path = PathBuf::from("/a/music/x.mp3");
    store.add("Mix", vec![plain.clone()]).unwrap();

    let mut dotted = entry("x");
    dotted.path = PathBuf::from("/a/music/../music/x.mp3");
    let mut dot = entry("x");
    dot.path = PathBuf::from("/a/./music/./x.mp3");
    assert_eq!(
        store.add("Mix", vec![dotted]),
        Ok(AddResult {
            added: 0,
            skipped: 1,
            total: 1
        })
    );
    assert_eq!(
        store.add("Mix", vec![dot]),
        Ok(AddResult {
            added: 0,
            skipped: 1,
            total: 1
        })
    );

    // A new file given with `..` is stored normalized, and a repeat within one add is skipped.
    let mut new_dotted = entry("y");
    new_dotted.path = PathBuf::from("/a/other/../music/y.mp3");
    let mut new_plain = entry("y");
    new_plain.path = PathBuf::from("/a/music/y.mp3");
    assert_eq!(
        store.add("Mix", vec![new_dotted, new_plain]),
        Ok(AddResult {
            added: 1,
            skipped: 1,
            total: 2
        })
    );
    assert_eq!(
        paths(&store.get("Mix").unwrap().entries),
        vec![PathBuf::from("/a/music/x.mp3"), PathBuf::from("/a/music/y.mp3")]
    );

    // A relative `..` path is made absolute and then normalized.
    let mut rel = entry("z");
    rel.path = PathBuf::from("rel/../z.mp3");
    store.add("Mix", vec![rel]).unwrap();
    assert!(worker.flush());
    let text = fs::read_to_string(dir.join("Mix.m3u8")).unwrap();
    assert!(!text.contains(".."), "file keeps `..`: {:?}", text);
    assert_eq!(
        on_disk(&dir, "Mix").entries[2].path,
        std::env::current_dir().unwrap().join("z.mp3")
    );
}

/// Issue #58: a hand-written relative entry is loaded normalized and the first edit writes it
/// back without `..`; adding the same file by its absolute path is a duplicate.
#[test]
fn test_hand_written_dot_dot_entry_is_written_back_normalized() {
    common::isolate();
    let dir = scratch("load_dot_dot");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Hand.m3u8"), "#EXTM3U\n../music/x.mp3\n").unwrap();
    let music = dir.parent().unwrap().join("music/x.mp3");

    let (worker, mut store) = open_store(&dir);
    assert_eq!(paths(&store.get("Hand").unwrap().entries), vec![music.clone()]);
    let mut same = entry("x");
    same.path = music.clone();
    assert_eq!(
        store.add("Hand", vec![same]),
        Ok(AddResult {
            added: 0,
            skipped: 1,
            total: 1
        })
    );
    store.add("Hand", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    let text = fs::read_to_string(dir.join("Hand.m3u8")).unwrap();
    assert!(!text.contains(".."), "file keeps `..`: {:?}", text);
    assert_eq!(paths(&on_disk(&dir, "Hand").entries), vec![music, entry("a").path]);
}

/// A line break in a path would split the M3U8 line and corrupt the file, so such entries
/// are skipped.
#[test]
fn test_add_skips_paths_with_line_breaks() {
    common::isolate();
    let dir = scratch("add_newline");
    let (worker, mut store) = open_store(&dir);
    store.create("Mix").unwrap();
    let mut lf = entry("lf");
    lf.path = PathBuf::from("/music/evil\n/etc/passwd.mp3");
    let mut cr = entry("cr");
    cr.path = PathBuf::from("/music/evil\r.mp3");
    assert_eq!(
        store.add("Mix", vec![lf, entry("a"), cr]),
        Ok(AddResult {
            added: 1,
            skipped: 2,
            total: 1
        })
    );
    assert!(worker.flush());
    assert_eq!(on_disk(&dir, "Mix").entries, vec![entry("a")]);
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
        Err(StoreError::Name(NameError::Reserved("Latin".to_string())))
    );
    assert_eq!(
        store.rename("other", "mix"),
        Err(StoreError::Name(NameError::Reserved("Mix".to_string())))
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
        Err(StoreError::Name(NameError::Reserved("mix".to_string())))
    );
    assert_eq!(names(&store), vec!["Mix"]);

    store.delete("Mix").unwrap();
    assert_eq!(
        store.create("mix"),
        Err(StoreError::Name(NameError::Reserved("mix".to_string())))
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
        Err(StoreError::Name(NameError::Reserved("Locked".to_string())))
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

/// NFC `Zoë` and the same name in NFD (e + combining diaeresis).
const ZOE_NFC: &str = "Zo\u{eb}";
const ZOE_NFD: &str = "Zoe\u{308}";

#[test]
fn test_create_refuses_a_name_differing_only_in_unicode_normalization() {
    common::isolate();
    // Issue #55: on APFS both names open the same file, so the create would overwrite it.
    let dir = scratch("nfd_create");
    let (worker, mut store) = open_store(&dir);
    store.create(ZOE_NFC).unwrap();
    store.add(ZOE_NFC, vec![entry("a")]).unwrap();

    assert_eq!(
        store.create(ZOE_NFD),
        Err(StoreError::Name(NameError::Duplicate(ZOE_NFC.to_string())))
    );
    assert_eq!(store.get(ZOE_NFD).map(|p| p.name.as_str()), Some(ZOE_NFC));
    assert!(worker.flush());
    assert_eq!(paths(&on_disk(&dir, ZOE_NFC).entries), paths(&[entry("a")]));
}

#[test]
fn test_create_refuses_a_name_equal_under_case_folding() {
    common::isolate();
    // `Straße` and `Strasse` are one file on APFS.
    let dir = scratch("casefold_create");
    let (worker, mut store) = open_store(&dir);
    store.create("Stra\u{df}e").unwrap();
    store.add("Stra\u{df}e", vec![entry("a")]).unwrap();
    assert_eq!(
        store.create("Strasse"),
        Err(StoreError::Name(NameError::Duplicate("Stra\u{df}e".to_string())))
    );
    assert!(worker.flush());
    assert_eq!(paths(&on_disk(&dir, "Stra\u{df}e").entries), paths(&[entry("a")]));
}

#[test]
fn test_rename_refuses_a_name_differing_only_in_unicode_normalization() {
    common::isolate();
    let dir = scratch("nfd_rename");
    let (_worker, mut store) = open_store(&dir);
    store.create(ZOE_NFC).unwrap();
    store.create("Other").unwrap();
    assert_eq!(
        store.rename("Other", ZOE_NFD),
        Err(StoreError::Name(NameError::Duplicate(ZOE_NFC.to_string())))
    );
}

#[test]
fn test_set_loaded_keeps_one_of_names_differing_only_in_unicode_normalization() {
    common::isolate();
    // Possible on a disk that keeps both forms (Linux): `Zoë.m3u8` twice, byte-different.
    let dir = scratch("nfd_load");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir, worker.handle());
    store.set_loaded(vec![
        Playlist::new(ZOE_NFD),
        Playlist::new("Zz"),
        Playlist::new(ZOE_NFC),
    ]);
    assert_eq!(names(&store).len(), 2, "one Zoë is dropped: {:?}", names(&store));
    // The dropped file is reserved, so creating either form is refused.
    assert!(store.create(ZOE_NFC).is_err());
    assert!(store.create(ZOE_NFD).is_err());
}

#[test]
fn test_skipped_file_reserves_its_name_in_either_normalization() {
    common::isolate();
    let dir = scratch("nfd_reserved");
    let (_worker, mut store) = open_store(&dir);
    store.reserve_names(vec![ZOE_NFC.to_string()]);
    assert!(matches!(
        store.create(ZOE_NFD),
        Err(StoreError::Name(NameError::Reserved(_)))
    ));
}

#[test]
fn test_load_dir_reserving_fails_when_the_folder_cannot_be_listed() {
    common::isolate();
    // Issue #56: an unlistable folder must not look empty, or a create would overwrite its files.
    let dir = scratch("unlistable");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("bom.m3u8"), "#EXTM3U\n/music/a.flac\n").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();

    // Root lists any folder, so the test proves nothing there.
    let listable = fs::read_dir(&dir).is_ok();
    let result = PlaylistStore::load_dir_reserving(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if !listable {
        assert!(result.is_err(), "expected an error, got {:?}", result);
    }
}

#[test]
fn test_rename_refuses_a_reserved_name_differing_only_in_normalization() {
    common::isolate();
    // A case-sensitive disk that ignores normalization (case-sensitive APFS) can hold `Zoë.m3u8`
    // and `zoë.m3u8` (NFD). `set_loaded` keeps `Zoë` and reserves the NFD `zoë`; renaming `Zoë`
    // to `zoë` typed as NFC would open the reserved file.
    let dir = scratch("nfd_reserved_rename");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir, worker.handle());
    store.set_loaded(vec![Playlist::new(ZOE_NFC), Playlist::new("zoe\u{308}")]);
    assert_eq!(names(&store), vec![ZOE_NFC]);
    assert!(matches!(
        store.rename(ZOE_NFC, "zo\u{eb}"),
        Err(StoreError::Name(NameError::Reserved(_)))
    ));
}

#[test]
fn test_load_dir_reserving_fails_on_a_dangling_symlink() {
    common::isolate();
    // A playlists folder linked to an unmounted volume is not "no playlists yet": loading it as
    // empty would let a create overwrite the real file once the volume mounts.
    let dir = scratch("dangling");
    let target = dir.parent().unwrap().join("unmounted/playlists");
    std::os::unix::fs::symlink(&target, &dir).unwrap();
    assert!(PlaylistStore::load_dir_reserving(&dir).is_err());
}

// ---------- writer failures reported back to the store (issues #62, #52) ----------

use quickstream::playlist_store::WriteFailure;

/// Put a file the store never read into the playlists folder, standing in for one its name rules
/// don't see (an HFS+, SMB or exFAT name match, or another running instance).
fn foreign_file(dir: &Path, file: &str, text: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(file);
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn test_create_onto_an_unseen_file_is_undone_and_the_file_kept() {
    common::isolate();
    let dir = scratch("create_collision");
    let (worker, mut store) = open_store(&dir);
    let theirs = "#EXTM3U\n#PLAYLIST:Mix\n/user/own.flac\n";
    let path = foreign_file(&dir, "Mix.m3u8", theirs);

    assert_eq!(store.create("Mix").unwrap(), "Mix");
    store.add("Mix", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        theirs,
        "the existing file was overwritten"
    );
    assert_eq!(
        failures,
        vec![WriteFailure::CreateUndone {
            name: "Mix".to_string(),
            message: "Couldn't create \"Mix\": a file with that name already exists".to_string(),
        }]
    );
    assert!(
        names(&store).is_empty(),
        "the playlist stays in memory: {:?}",
        names(&store)
    );
    // The name is now known to be taken.
    assert_eq!(
        store.create("Mix"),
        Err(StoreError::Name(NameError::Reserved("Mix".to_string())))
    );
    assert!(worker.flush());
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
    assert_eq!(file_names(&dir), vec!["Mix.m3u8".to_string()]);
}

#[test]
fn test_deleting_a_playlist_whose_create_failed_keeps_the_file() {
    common::isolate();
    let dir = scratch("create_collision_delete");
    let (worker, mut store) = open_store(&dir);
    let path = foreign_file(&dir, "Mix.m3u8", "theirs");

    store.create("Mix").unwrap();
    store.delete("Mix").unwrap(); // before the failure arrives
    assert!(worker.flush());
    assert!(store.poll_write_failures().is_empty(), "nothing left to undo");
    assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
}

#[test]
fn test_failed_rename_reverts_the_name_and_later_edits_write_the_real_file() {
    common::isolate();
    let dir = scratch("rename_failure");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    store.add("Old", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    let theirs = foreign_file(&dir, "New.m3u8", "theirs");

    assert_eq!(store.rename("Old", "New").unwrap(), "New");
    store.add("New", vec![entry("b")]).unwrap(); // sent before the failure arrives
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert_eq!(
        failures,
        vec![WriteFailure::RenameUndone {
            attempted: "New".to_string(),
            restored: "Old".to_string(),
            message: "Couldn't rename \"Old\" to \"New\": a file with that name already exists".to_string(),
        }]
    );
    assert_eq!(names(&store), vec!["Old"]);
    store.add("Old", vec![entry("c")]).unwrap();
    assert!(worker.flush());
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs");
    assert_eq!(file_names(&dir), vec!["New.m3u8".to_string(), "Old.m3u8".to_string()]);
    let old = on_disk(&dir, "Old");
    assert_eq!(paths(&old.entries), paths(&[entry("a"), entry("b"), entry("c")]));
    assert!(fs::read_to_string(dir.join("Old.m3u8"))
        .unwrap()
        .contains("#PLAYLIST:Old\n"));
}

/// A second rename sent before the first one's failure arrived is refused by the writer too;
/// the playlist goes back to the name its file really has.
#[test]
fn test_rename_chain_after_a_failed_rename_reverts_to_the_real_file() {
    common::isolate();
    let dir = scratch("rename_failure_chain");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    assert!(worker.flush());
    foreign_file(&dir, "New.m3u8", "theirs");

    store.rename("Old", "New").unwrap();
    store.rename("New", "Newer").unwrap();
    store.add("Newer", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert_eq!(failures.len(), 1, "{:?}", failures);
    assert!(
        matches!(&failures[0], WriteFailure::RenameUndone { attempted, restored, .. }
        if attempted == "Newer" && restored == "Old")
    );
    assert_eq!(names(&store), vec!["Old"]);
    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["New.m3u8".to_string(), "Old.m3u8".to_string()]);
    assert_eq!(paths(&on_disk(&dir, "Old").entries), paths(&[entry("a")]));
}

/// After quit the writer is gone and operations run on the calling thread; a failed rename is
/// still reported and undone.
#[test]
fn test_failed_rename_after_the_writer_shut_down_is_reverted() {
    common::isolate();
    let dir = scratch("rename_failure_late");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    drop(worker);
    let theirs = foreign_file(&dir, "New.m3u8", "theirs");

    store.rename("Old", "New").unwrap();
    store.add("New", vec![entry("a")]).unwrap();
    let failures = store.poll_write_failures();

    assert!(matches!(failures.as_slice(), [WriteFailure::RenameUndone { restored, .. }] if restored == "Old"));
    assert_eq!(names(&store), vec!["Old"]);
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs");
    assert_eq!(paths(&on_disk(&dir, "Old").entries), paths(&[entry("a")]));
}

/// A rename the OS refuses (a read-only playlists folder) is undone the same way.
#[test]
fn test_rename_refused_by_permissions_is_reverted() {
    common::isolate();
    let dir = scratch("rename_failure_perm");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    assert!(worker.flush());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(dir.join("probe"), b"").is_ok() {
        // Running as root: permissions don't apply, nothing to test.
        let _ = fs::remove_file(dir.join("probe"));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    store.rename("Old", "New").unwrap();
    assert!(worker.flush());
    let failures = store.poll_write_failures();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        matches!(failures.as_slice(), [WriteFailure::RenameUndone { restored, message, .. }]
        if restored == "Old" && message.starts_with("Couldn't rename \"Old\" to \"New\": "))
    );
    assert_eq!(names(&store), vec!["Old"]);
    assert_eq!(file_names(&dir), vec!["Old.m3u8".to_string()]);
}

// ---------- names a create or rename still being saved starts from or goes to (PR #65 review) ----------

/// The status a refused create or rename shows, as the UI and the dashboard print it.
fn refusal<T: std::fmt::Debug>(result: Result<T, StoreError>) -> String {
    result.expect_err("should be refused").to_string()
}

/// CC1 / SP Important 1: rename A -> B will fail (a file the store never saw is at B). Renaming
/// another playlist onto "A" before that failure is heard would leave two playlists claiming
/// A.m3u8: the undo used to drop the other playlist, drop the restored playlist's saves and make
/// deleting "A" remove C.m3u8. The second rename is refused until the writer has finished.
#[test]
fn test_rename_onto_the_old_name_of_a_rename_still_saving_is_refused() {
    common::isolate();
    let dir = scratch("pending_rename_onto_old");
    let (worker, mut store) = open_store(&dir);
    store.create("A").unwrap();
    store.add("A", vec![entry("a")]).unwrap();
    store.create("C").unwrap();
    store.add("C", vec![entry("c")]).unwrap();
    assert!(worker.flush());
    assert!(store.poll_write_failures().is_empty());
    let theirs = foreign_file(&dir, "B.m3u8", "theirs");

    store.rename("A", "B").unwrap();
    assert_eq!(
        refusal(store.rename("C", "A")),
        "Still renaming \"A\" to \"B\", try again"
    );
    assert_eq!(names(&store), vec!["B", "C"], "the refused rename changed nothing");
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert!(
        matches!(failures.as_slice(), [WriteFailure::RenameUndone { attempted, restored, message }]
        if attempted == "B" && restored == "A"
            && message == "Couldn't rename \"A\" to \"B\": a file with that name already exists"),
        "{:?}",
        failures
    );
    assert_eq!(names(&store), vec!["A", "C"], "no playlist disappeared");
    store.add("A", vec![entry("a2")]).unwrap();
    store.add("C", vec![entry("c2")]).unwrap();
    assert!(worker.flush());
    assert_eq!(paths(&on_disk(&dir, "A").entries), paths(&[entry("a"), entry("a2")]));
    assert_eq!(paths(&on_disk(&dir, "C").entries), paths(&[entry("c"), entry("c2")]));

    // Deleting "A" removes A.m3u8, never C.m3u8 or the file in the way.
    store.delete("A").unwrap();
    assert!(worker.flush());
    assert_eq!(file_names(&dir), vec!["B.m3u8".to_string(), "C.m3u8".to_string()]);
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs");
    assert_eq!(paths(&on_disk(&dir, "C").entries), paths(&[entry("c"), entry("c2")]));
}

/// CC1 (second variant) / SP Important 2: creating a playlist with the old name while the rename
/// away from it is still being saved. The create used to fail on the playlist's own Old.m3u8,
/// mark that file as not ours, drop every later save of the restored playlist and never report
/// the create failure. It is refused instead, and the restored playlist keeps saving.
#[test]
fn test_create_onto_the_old_name_of_a_rename_still_saving_is_refused() {
    common::isolate();
    let dir = scratch("pending_rename_create_old");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    store.add("Old", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    foreign_file(&dir, "New.m3u8", "theirs");

    store.rename("Old", "New").unwrap();
    assert_eq!(
        refusal(store.create("Old")),
        "Still renaming \"Old\" to \"New\", try again"
    );
    assert_eq!(
        refusal(store.create("old")),
        "Still renaming \"Old\" to \"New\", try again",
        "names compare as on disk"
    );
    store.add("New", vec![entry("b")]).unwrap();
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert_eq!(
        failures,
        vec![WriteFailure::RenameUndone {
            attempted: "New".to_string(),
            restored: "Old".to_string(),
            message: "Couldn't rename \"Old\" to \"New\": a file with that name already exists".to_string(),
        }]
    );
    assert_eq!(names(&store), vec!["Old"]);
    store.add("Old", vec![entry("c")]).unwrap();
    assert!(worker.flush());
    assert_eq!(
        paths(&on_disk(&dir, "Old").entries),
        paths(&[entry("a"), entry("b"), entry("c")]),
        "every save of the restored playlist reached its file"
    );
    assert_eq!(fs::read_to_string(dir.join("New.m3u8")).unwrap(), "theirs");
    assert_eq!(
        store.create("Old"),
        Err(StoreError::Name(NameError::Duplicate("Old".to_string())))
    );
}

/// CC2: renaming a playlist straight back to its old name before the failure of the rename away
/// from it is heard used to leave that name marked as not ours in the writer, so every later save
/// was dropped. Renaming back is the playlist's own business, so it isn't refused: the writer
/// sees it lands on the real file, saves reach that file again, and the failure is still reported.
#[test]
fn test_rename_back_while_the_rename_away_is_still_saving_keeps_saving() {
    common::isolate();
    let dir = scratch("pending_rename_back");
    let (worker, mut store) = open_store(&dir);
    store.create("A").unwrap();
    store.add("A", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    foreign_file(&dir, "B.m3u8", "theirs");

    store.rename("A", "B").unwrap();
    assert_eq!(store.rename("B", "A"), Ok("A".to_string()));
    store.add("A", vec![entry("b")]).unwrap();
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert_eq!(
        failures,
        vec![WriteFailure::RenameUndone {
            attempted: "A".to_string(),
            restored: "A".to_string(),
            message: "Couldn't rename \"A\" to \"B\": a file with that name already exists".to_string(),
        }]
    );
    assert_eq!(names(&store), vec!["A"]);
    store.add("A", vec![entry("c")]).unwrap();
    assert!(worker.flush());
    assert_eq!(
        paths(&on_disk(&dir, "A").entries),
        paths(&[entry("a"), entry("b"), entry("c")])
    );
    assert_eq!(fs::read_to_string(dir.join("B.m3u8")).unwrap(), "theirs");
    assert_eq!(file_names(&dir), vec!["A.m3u8".to_string(), "B.m3u8".to_string()]);
}

/// Once the playlist with a pending rename is deleted, its names are free: the delete removed
/// the file still at the old name, so another playlist can take that name right away.
#[test]
fn test_names_of_a_deleted_playlist_are_free_while_its_rename_is_still_saving() {
    common::isolate();
    let dir = scratch("pending_rename_deleted");
    let (worker, mut store) = open_store(&dir);
    store.create("A").unwrap();
    store.add("A", vec![entry("a")]).unwrap();
    store.create("C").unwrap();
    store.add("C", vec![entry("c")]).unwrap();
    assert!(worker.flush());
    foreign_file(&dir, "B.m3u8", "theirs");

    store.rename("A", "B").unwrap();
    store.delete("B").unwrap();
    store.rename("C", "A").unwrap();
    store.add("A", vec![entry("c2")]).unwrap();
    assert!(worker.flush());

    assert!(
        store.poll_write_failures().is_empty(),
        "the deleted playlist has nothing to undo"
    );
    assert_eq!(names(&store), vec!["A"]);
    assert_eq!(file_names(&dir), vec!["A.m3u8".to_string(), "B.m3u8".to_string()]);
    assert_eq!(paths(&on_disk(&dir, "A").entries), paths(&[entry("c"), entry("c2")]));
    assert_eq!(fs::read_to_string(dir.join("B.m3u8")).unwrap(), "theirs");
}

/// A create that will fail keeps its name busy too: the playlist renamed away from it is removed
/// when the failure arrives, and a second create of the name waits instead of failing the same
/// way unreported.
#[test]
fn test_create_of_a_name_whose_create_is_still_saving_is_refused() {
    common::isolate();
    let dir = scratch("pending_create");
    let (worker, mut store) = open_store(&dir);
    let theirs = foreign_file(&dir, "Mix.m3u8", "theirs");

    store.create("Mix").unwrap();
    store.rename("Mix", "Other").unwrap(); // a new name nothing is saving to: allowed
    assert_eq!(refusal(store.create("Mix")), "Still creating \"Mix\", try again");
    assert!(worker.flush());
    let failures = store.poll_write_failures();

    assert!(
        matches!(failures.as_slice(), [WriteFailure::CreateUndone { name, message }]
        if name == "Other" && message == "Couldn't create \"Mix\": a file with that name already exists"),
        "{:?}",
        failures
    );
    assert!(names(&store).is_empty());
    assert_eq!(
        store.create("Mix"),
        Err(StoreError::Name(NameError::Reserved("Mix".to_string())))
    );
    store.create("Other").unwrap();
    store.add("Other", vec![entry("a")]).unwrap();
    assert!(worker.flush());
    assert!(store.poll_write_failures().is_empty());
    assert_eq!(file_names(&dir), vec!["Mix.m3u8".to_string(), "Other.m3u8".to_string()]);
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs");
    assert_eq!(paths(&on_disk(&dir, "Other").entries), paths(&[entry("a")]));
}

/// The normal path: with no failures, nothing waits on the UI to poll. Edits of names nothing is
/// saving to go straight through, and once the writer has finished, a name a rename just left
/// is free again.
#[test]
fn test_without_failures_creates_and_renames_are_not_held_up() {
    common::isolate();
    let dir = scratch("pending_fast_path");
    let (worker, mut store) = open_store(&dir);

    store.create("A").unwrap();
    store.rename("A", "B").unwrap();
    store.add("B", vec![entry("b")]).unwrap();
    store.create("C").unwrap();
    store.rename("C", "D").unwrap();
    store.rename("D", "E").unwrap();
    store.add("E", vec![entry("e")]).unwrap();
    assert!(worker.flush());

    // No poll_write_failures in between: the writer's acknowledgements are enough.
    store.create("A").unwrap();
    store.add("A", vec![entry("a")]).unwrap();
    store.rename("E", "C").unwrap();
    assert!(worker.flush());
    store.rename("B", "D").unwrap();
    assert!(worker.flush());

    assert!(store.poll_write_failures().is_empty());
    assert_eq!(names(&store), vec!["A", "C", "D"]);
    assert_eq!(
        file_names(&dir),
        vec!["A.m3u8".to_string(), "C.m3u8".to_string(), "D.m3u8".to_string()]
    );
    assert_eq!(paths(&on_disk(&dir, "A").entries), paths(&[entry("a")]));
    assert_eq!(paths(&on_disk(&dir, "C").entries), paths(&[entry("e")]));
    assert_eq!(paths(&on_disk(&dir, "D").entries), paths(&[entry("b")]));
}

// ---------- stable ids for work that outlives a rename (issue #52) ----------

/// A folder add reads tags on a worker and adds when the listing arrives. It names its target
/// by id, so a rename in the meantime moves the add with the playlist.
#[test]
fn test_id_follows_renames_and_ends_with_delete() {
    common::isolate();
    let dir = scratch("ids");
    // A playlist loaded from disk has no id until asked for one.
    foreign_file(&dir, "Loaded.m3u8", "#EXTM3U\n");
    let (worker, mut store) = open_store(&dir);
    store.create("Road Trip").unwrap();

    let loaded = store.id_of("loaded").expect("loaded playlists get an id on demand");
    let road_trip = store.id_of("Road Trip").expect("created playlists have an id");
    assert_ne!(loaded, road_trip);
    assert_eq!(store.id_of("loaded"), Some(loaded), "asking again gives the same id");
    assert_eq!(store.id_of("Missing"), None);
    assert_eq!(store.name_of(loaded), Some("Loaded"));

    store.rename("Road Trip", "Summer").unwrap();
    assert_eq!(store.name_of(road_trip), Some("Summer"));
    store.rename("Loaded", "Kept").unwrap();
    assert_eq!(store.name_of(loaded), Some("Kept"));

    store.delete("Summer").unwrap();
    assert_eq!(store.name_of(road_trip), None);
    // A new playlist under the old name is a different playlist.
    store.create("Summer").unwrap();
    assert_eq!(store.name_of(road_trip), None);
    assert_ne!(store.id_of("Summer"), Some(road_trip));
    assert!(worker.flush());
}

/// A failed rename gives the playlist its old name back; its id goes with it.
#[test]
fn test_id_follows_a_rename_the_writer_undid() {
    common::isolate();
    let dir = scratch("ids_rename_failure");
    let (worker, mut store) = open_store(&dir);
    store.create("Old").unwrap();
    assert!(worker.flush());
    foreign_file(&dir, "New.m3u8", "theirs");
    let id = store.id_of("Old").unwrap();

    store.rename("Old", "New").unwrap();
    assert_eq!(store.name_of(id), Some("New"));
    assert!(worker.flush());
    assert_eq!(store.poll_write_failures().len(), 1);
    assert_eq!(store.name_of(id), Some("Old"));
}

/// Issue #50: the web dashboard's snapshot is rebuilt when the revision changes, and edits from
/// the dashboard carry the revision they were made against. Every change to the playlists bumps
/// it; anything that changes nothing leaves it alone.
#[test]
fn test_revision_changes_with_every_edit_and_only_then() {
    common::isolate();
    let dir = scratch("revision");
    let worker = PersistenceWorker::spawn();
    let mut store = PlaylistStore::new(dir.clone(), worker.handle());
    let unloaded = store.revision();
    store.set_loaded(Vec::new());
    let mut last = store.revision();
    assert_ne!(last, unloaded, "loading changes what the store holds");

    let mut changed = |store: &PlaylistStore, what: &str| {
        let now = store.revision();
        assert_ne!(now, last, "{} did not bump the revision", what);
        last = now;
    };
    store.create("Road Trip").unwrap();
    changed(&store, "create");
    store.add("Road Trip", vec![entry("a"), entry("b")]).unwrap();
    changed(&store, "add");
    store.move_entry("Road Trip", 0, MoveDir::Down).unwrap();
    changed(&store, "move");
    store.remove("Road Trip", 0).unwrap();
    changed(&store, "remove");
    store.rename("Road Trip", "Summer").unwrap();
    changed(&store, "rename");
    store.delete("Summer").unwrap();
    changed(&store, "delete");

    // No-ops and refusals leave it alone.
    store.create("Mix").unwrap();
    store.add("Mix", vec![entry("a")]).unwrap();
    let before = store.revision();
    store.add("Mix", vec![entry("a")]).unwrap();
    store.move_entry("Mix", 0, MoveDir::Up).unwrap();
    store.rename("Mix", "Mix").unwrap();
    let _ = store.create("mix");
    let _ = store.remove("Mix", 5);
    let _ = store.delete("Nope");
    assert_eq!(store.revision(), before);
    assert!(worker.flush());
}

/// Undoing a create or rename the writer couldn't do changes the playlists too.
#[test]
fn test_revision_changes_when_a_failed_write_is_undone() {
    common::isolate();
    let dir = scratch("revision_undo");
    let (worker, mut store) = open_store(&dir);
    foreign_file(&dir, "Mix.m3u8", "theirs");
    store.create("Mix").unwrap();
    assert!(worker.flush());
    let before = store.revision();
    assert_eq!(store.poll_write_failures().len(), 1);
    assert_ne!(store.revision(), before);
}
