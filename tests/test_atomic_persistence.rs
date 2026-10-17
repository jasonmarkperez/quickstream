//! Issue #17: the library cache, settings and bookmarks are written atomically, cache writers
//! never interleave, and a corrupt config is backed up before defaults are used.
//!
//! Every test uses explicit temp paths; nothing here touches the user's real config or cache.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quickstream::library::LibraryManager;
use quickstream::metadata::{fallback_metadata, AudioMetadata};
use quickstream::settings::SettingsManager;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_atomic_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn track(name: &str) -> AudioMetadata {
    fallback_metadata(Path::new(&format!("/nonexistent/qs-test/{}.flac", name)))
}

fn tmp_leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect()
}

// ---------- library cache ----------

#[test]
fn test_save_cache_replaces_file_by_rename_not_truncation() {
    common::isolate();
    let dir = scratch("cache_rename");
    let index = dir.join("music_index.jsonl");
    assert!(LibraryManager::save_cache_to(&index, &[track("a")]));
    let before = fs::metadata(&index).unwrap().ino();

    assert!(LibraryManager::save_cache_to(&index, &[track("a"), track("b")]));
    let after = fs::metadata(&index).unwrap().ino();

    assert_ne!(before, after, "save_cache must write a temp file and rename it over the cache");
    assert_eq!(LibraryManager::load_cache_from(&index).len(), 2);
    assert!(tmp_leftovers(&dir).is_empty(), "temp file left behind: {:?}", tmp_leftovers(&dir));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_save_cache_keeps_entries_appended_since_the_snapshot() {
    common::isolate();
    // A scan snapshots the cache, then metadata workers append, then the scan saves its snapshot.
    // The appended entries must survive the save.
    let dir = scratch("cache_merge");
    let index = dir.join("music_index.jsonl");
    LibraryManager::append_to_cache_at(&index, &track("old"));
    let snapshot = LibraryManager::load_cache_from(&index);

    LibraryManager::append_to_cache_at(&index, &track("appended_meanwhile"));
    let mut tracks = snapshot;
    tracks.push(track("scanned"));
    assert!(LibraryManager::save_cache_to(&index, &tracks));

    let names: Vec<String> = LibraryManager::load_cache_from(&index).into_iter().map(|t| t.filename).collect();
    for want in ["old.flac", "scanned.flac", "appended_meanwhile.flac"] {
        assert!(names.iter().any(|n| n == want), "{} missing from {:?}", want, names);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_concurrent_appends_and_saves_never_interleave_or_lose_lines() {
    common::isolate();
    let dir = scratch("cache_concurrent");
    let index = Arc::new(dir.join("music_index.jsonl"));
    // Long fields make each line span several write() calls if a writer isn't serialized.
    let long = "x".repeat(6000);

    let mut handles = Vec::new();
    for t in 0..6 {
        let index = index.clone();
        let long = long.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..40 {
                let mut m = track(&format!("t{}_{}", t, i));
                m.genre = long.clone();
                LibraryManager::append_to_cache_at(&index, &m);
            }
        }));
    }
    {
        let index = index.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..20 {
                let snapshot = LibraryManager::load_cache_from(&index);
                LibraryManager::save_cache_to(&index, &snapshot);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let raw = fs::read(&*index).unwrap();
    let mut parsed = std::collections::HashSet::new();
    for line in raw.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let m: AudioMetadata = serde_json::from_slice(line)
            .unwrap_or_else(|e| panic!("corrupt line ({}): {:.80}", e, String::from_utf8_lossy(line)));
        parsed.insert(m.filename);
    }
    for t in 0..6 {
        for i in 0..40 {
            let name = format!("t{}_{}.flac", t, i);
            assert!(parsed.contains(&name), "{} lost", name);
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_load_skips_partial_last_line_including_truncated_utf8() {
    common::isolate();
    let dir = scratch("cache_partial");
    let index = dir.join("music_index.jsonl");
    let good = serde_json::to_string(&track("good")).unwrap();
    let mut partial = serde_json::to_vec(&track("Beyoncé")).unwrap();
    // Cut the line in the middle of the two-byte "é", as a crash mid-write could.
    let cut = partial.windows(2).position(|w| w == "é".as_bytes()).unwrap() + 1;
    partial.truncate(cut);
    let mut bytes = format!("{}\n", good).into_bytes();
    bytes.extend_from_slice(&partial);
    fs::write(&index, &bytes).unwrap();

    let loaded = LibraryManager::load_cache_from(&index);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].filename, "good.flac");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_bad_line_in_the_middle_does_not_hide_later_entries() {
    common::isolate();
    let dir = scratch("cache_middle");
    let index = dir.join("music_index.jsonl");
    let mut f = fs::File::create(&index).unwrap();
    writeln!(f, "{}", serde_json::to_string(&track("first")).unwrap()).unwrap();
    f.write_all(b"{\"filepath\":\"/x/\xc3\n").unwrap(); // invalid UTF-8, unparsable
    writeln!(f, "{}", serde_json::to_string(&track("last")).unwrap()).unwrap();
    drop(f);

    let names: Vec<String> = LibraryManager::load_cache_from(&index).into_iter().map(|t| t.filename).collect();
    assert_eq!(names, vec!["first.flac".to_string(), "last.flac".to_string()]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_append_after_crash_mid_line_starts_a_new_line() {
    common::isolate();
    let dir = scratch("cache_append_after_partial");
    let index = dir.join("music_index.jsonl");
    let good = serde_json::to_string(&track("good")).unwrap();
    fs::write(&index, format!("{}\n{{\"filepath\":\"/half", good)).unwrap();

    LibraryManager::append_to_cache_at(&index, &track("after_crash"));

    let names: Vec<String> = LibraryManager::load_cache_from(&index).into_iter().map(|t| t.filename).collect();
    assert_eq!(names, vec!["good.flac".to_string(), "after_crash.flac".to_string()]);
    let _ = fs::remove_dir_all(&dir);
}

// ---------- settings ----------

#[test]
fn test_settings_save_is_atomic_rename() {
    common::isolate();
    let dir = scratch("settings_rename");
    let path = dir.join("config.json");
    let mut mgr = SettingsManager::with_path(path.clone());
    assert!(mgr.save());
    let before = fs::metadata(&path).unwrap().ino();
    mgr.settings.volume_step = 10;
    assert!(mgr.save());
    assert_ne!(before, fs::metadata(&path).unwrap().ino(), "settings must be replaced via rename");
    assert_eq!(SettingsManager::load_from(&path).unwrap().volume_step, 10);
    assert!(tmp_leftovers(&dir).is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_corrupt_settings_are_backed_up_before_defaults_are_used() {
    common::isolate();
    let dir = scratch("settings_corrupt");
    let path = dir.join("config.json");
    let bad = b"{\"metadata_view\": false, \"volume_step\": 10, oops".to_vec();
    fs::write(&path, &bad).unwrap();

    let mgr = SettingsManager::with_path(path.clone());
    assert_eq!(mgr.settings, quickstream::settings::Settings::default());
    assert_eq!(fs::read(dir.join("config.json.bak")).unwrap(), bad, "bad file copied to config.json.bak");
    assert_eq!(fs::read(&path).unwrap(), bad, "loading never rewrites the user's file");

    // The next save replaces config.json but the backup stays.
    assert!(mgr.save());
    assert_eq!(fs::read(dir.join("config.json.bak")).unwrap(), bad);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_existing_backup_is_never_overwritten() {
    common::isolate();
    let dir = scratch("settings_bak_keep");
    let path = dir.join("config.json");
    let first_bad = b"{ first broken edit".to_vec();
    fs::write(&path, &first_bad).unwrap();
    let _ = SettingsManager::with_path(path.clone());
    // Loading the same broken file again must not pile up duplicate backups.
    let _ = SettingsManager::with_path(path.clone());
    let backups = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(d)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("config.json.bak"))
            .collect();
        v.sort();
        v
    };
    assert_eq!(backups(&dir).len(), 1);

    let second_bad = b"{ second broken edit".to_vec();
    fs::write(&path, &second_bad).unwrap();
    let _ = SettingsManager::with_path(path.clone());

    assert_eq!(fs::read(dir.join("config.json.bak")).unwrap(), first_bad, "earliest backup kept");
    let all = backups(&dir);
    assert_eq!(all.len(), 2, "second corrupt file gets its own backup: {:?}", all);
    assert!(all.iter().any(|p| fs::read(p).unwrap() == second_bad));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_missing_or_empty_settings_make_no_backup() {
    common::isolate();
    let dir = scratch("settings_missing");
    let path = dir.join("config.json");
    let _ = SettingsManager::with_path(path.clone());
    fs::write(&path, b"").unwrap();
    let _ = SettingsManager::with_path(path.clone());
    assert!(!dir.join("config.json.bak").exists());
    let _ = fs::remove_dir_all(&dir);
}

// ---------- shared helpers ----------

#[test]
fn test_atomic_write_failure_leaves_original_untouched() {
    common::isolate();
    let dir = scratch("atomic_fail");
    let path = dir.join("bookmarks.json");
    fs::write(&path, b"[\"/keep\"]").unwrap();
    // A directory squatting on the temp path makes the temp-file write fail.
    fs::create_dir_all(quickstream::persist::tmp_path_for(&path)).unwrap();
    assert!(quickstream::persist::atomic_write(&path, b"[]").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"[\"/keep\"]");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_corrupt_bookmarks_are_backed_up_by_the_shared_loader() {
    common::isolate();
    let dir = scratch("bookmarks_corrupt");
    let path = dir.join("bookmarks.json");
    fs::write(&path, b"[\"/a\", ").unwrap();
    let loaded: Option<Vec<String>> = quickstream::persist::load_json_or_backup(&path, "bookmarks");
    assert!(loaded.is_none());
    assert_eq!(fs::read(dir.join("bookmarks.json.bak")).unwrap(), b"[\"/a\", ");

    fs::write(&path, b"[\"/a\"]").unwrap();
    let loaded: Option<Vec<String>> = quickstream::persist::load_json_or_backup(&path, "bookmarks");
    assert_eq!(loaded, Some(vec!["/a".to_string()]));
    let _ = fs::remove_dir_all(&dir);
}
