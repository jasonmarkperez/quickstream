//! The background library indexer (#41, `analysis::index`): it walks a library tree in a temp
//! folder, reads the tags of files that changed into tag-only rows, records each folder, and
//! never touches the library. Every test runs it on its own database in a scratch folder.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use quickstream::analysis::db::TrackFeatures;
use quickstream::analysis::index::{
    default_tag_reader, folder_mtime, run_library_index, IndexConfig, IndexControl, IndexProgress, IndexSnapshot,
    IndexState, IndexTagReader,
};
use quickstream::analysis::worker::TagReadError;
use quickstream::analysis::AnalysisDatabase;
use quickstream::ui::workers::WorkerRegistry;

/// A scratch folder for one test: `lib` (the library) and `db` (the analysis database) side by
/// side, so nothing the indexer writes can land in the library.
fn scratch(name: &str) -> (PathBuf, PathBuf) {
    let dir = common::isolate().join(format!("index-{name}"));
    let _ = fs::remove_dir_all(&dir);
    let lib = dir.join("lib");
    fs::create_dir_all(&lib).unwrap();
    fs::create_dir_all(dir.join("db")).unwrap();
    (lib, dir.join("db").join("track_analysis.db"))
}

fn silent_wav(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let data_len = 800u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&8000u32.to_le_bytes());
    b.extend_from_slice(&16000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    b.resize(b.len() + data_len as usize, 0);
    fs::write(path, b).unwrap();
}

/// A silent WAV with an ID3v2 title, artist and BPM tag.
fn tagged_wav(path: &Path, title: &str, bpm: &str) {
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::tag::{ItemKey, Tag, TagType};

    silent_wav(path);
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.to_string());
    tag.set_artist("Tag Artist".to_string());
    tag.insert_text(ItemKey::IntegerBpm, bpm.to_string());
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

/// `artists` artist folders with an album of `per_album` tracks each, and a loose track.
fn library_tree(lib: &Path, artists: usize, per_album: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for a in 0..artists {
        for t in 0..per_album {
            let path = lib
                .join(format!("Artist {a:02}"))
                .join("Album")
                .join(format!("{t:02} Song.wav"));
            silent_wav(&path);
            files.push(path);
        }
    }
    let loose = lib.join("loose.wav");
    silent_wav(&loose);
    files.push(loose);
    files
}

fn config(roots: Vec<PathBuf>) -> IndexConfig {
    IndexConfig {
        start_delay: Duration::ZERO,
        rescan_interval: Duration::from_secs(3600),
        ..IndexConfig::new(roots)
    }
}

/// A reader that counts its reads (and records their order) and then reads as the app does.
fn counting_reader(count: Arc<AtomicUsize>, order: Arc<Mutex<Vec<PathBuf>>>) -> IndexTagReader {
    let real = default_tag_reader();
    Arc::new(move |path: &Path, stamp| {
        count.fetch_add(1, Ordering::SeqCst);
        order.lock().unwrap().push(path.to_path_buf());
        real(path, stamp)
    })
}

struct Running {
    control: Arc<IndexControl>,
    progress: Arc<IndexProgress>,
    handle: std::thread::JoinHandle<()>,
}

fn spawn(db_path: &Path, cfg: IndexConfig, reader: IndexTagReader) -> Running {
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let (c, p, path) = (control.clone(), progress.clone(), db_path.to_path_buf());
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), cfg, &c, &p, reader);
    });
    Running {
        control,
        progress,
        handle,
    }
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Running {
    fn wait_state(&self, state: IndexState) {
        wait_for(&format!("{state:?}"), Duration::from_secs(20), || {
            self.progress.state() == state
        });
    }

    fn finish(self) -> IndexSnapshot {
        let snapshot = self.progress.snapshot();
        self.control.stop();
        self.handle.join().unwrap();
        snapshot
    }
}

/// One whole pass with `reader`; the snapshot when it is done.
fn one_pass(db_path: &Path, roots: Vec<PathBuf>, reader: IndexTagReader) -> IndexSnapshot {
    one_pass_with(db_path, config(roots), reader)
}

/// One whole pass with `cfg` and `reader`; the snapshot when it is done.
fn one_pass_with(db_path: &Path, cfg: IndexConfig, reader: IndexTagReader) -> IndexSnapshot {
    let run = spawn(db_path, cfg, reader);
    run.wait_state(IndexState::Done);
    run.finish()
}

fn rows_under(db_path: &Path, dir: &Path) -> Vec<TrackFeatures> {
    AnalysisDatabase::open_at(db_path).unwrap().tracks_under(dir, 64)
}

/// Every entry under `dir` (folders included): its kind, size, mtime and a hash of its contents.
fn tree_snapshot(dir: &Path) -> BTreeMap<PathBuf, (bool, u64, SystemTime, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let md = fs::symlink_metadata(entry.path()).unwrap();
            let hash = if md.is_file() {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                fs::read(entry.path()).unwrap().hash(&mut h);
                h.finish()
            } else {
                0
            };
            if md.is_dir() {
                stack.push(entry.path());
            }
            out.insert(entry.path(), (md.is_dir(), md.len(), md.modified().unwrap(), hash));
        }
    }
    out.insert(
        dir.to_path_buf(),
        (true, 0, fs::metadata(dir).unwrap().modified().unwrap(), 0),
    );
    out
}

// ---------- zero-touch ----------

/// A pass reads the library and writes only the analysis database: every name, size, mtime and
/// content hash in the tree is the same afterwards, and no file appears in it.
#[test]
fn a_pass_leaves_the_library_unchanged() {
    common::isolate();
    let (lib, db_path) = scratch("zero_touch");
    library_tree(&lib, 3, 3);
    tagged_wav(&lib.join("Artist 00").join("Tagged.wav"), "Tagged", "124");
    fs::write(lib.join("Artist 01").join("cover.jpg"), b"not audio").unwrap();
    fs::write(lib.join("Artist 02").join("broken.mp3"), b"not really an mp3").unwrap();
    let before = tree_snapshot(&lib);

    let snap = one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    assert_eq!(snap.read, 12, "{snap:?}");
    assert_eq!(snap.failed, 0, "{snap:?}");
    assert_eq!(tree_snapshot(&lib), before, "the library changed");
    // And a second pass, which reads nothing, leaves it as it is too.
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    assert_eq!(tree_snapshot(&lib), before, "the library changed");
}

// ---------- incremental ----------

/// A second pass reads no tags; a file touched since is read once.
#[test]
fn a_second_pass_reads_only_what_changed() {
    common::isolate();
    let (lib, db_path) = scratch("incremental");
    let files = library_tree(&lib, 3, 4);
    let count = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));

    let first = one_pass(
        &db_path,
        vec![lib.clone()],
        counting_reader(count.clone(), order.clone()),
    );
    assert_eq!(count.load(Ordering::SeqCst), files.len());
    assert_eq!(
        (first.read, first.unchanged, first.top_done, first.top_total),
        (13, 0, 3, 3)
    );
    assert_eq!(rows_under(&db_path, &lib).len(), files.len());

    let second = one_pass(
        &db_path,
        vec![lib.clone()],
        counting_reader(count.clone(), order.clone()),
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        files.len(),
        "no tag read on an unchanged library"
    );
    assert_eq!((second.read, second.unchanged), (0, 13));

    // Touch one file (a new mtime): it alone is read again.
    let touched = &files[5];
    let file = fs::File::options().write(true).open(touched).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(120)).unwrap();
    drop(file);
    order.lock().unwrap().clear();
    let third = one_pass(
        &db_path,
        vec![lib.clone()],
        counting_reader(count.clone(), order.clone()),
    );
    assert_eq!((third.read, third.unchanged), (1, 12));
    assert_eq!(*order.lock().unwrap(), vec![touched.clone()]);
}

/// Folders are recorded with their audio file count, whole folders and subtrees as complete.
#[test]
fn folders_are_recorded_and_whole_subtrees_marked() {
    common::isolate();
    let (lib, db_path) = scratch("records");
    library_tree(&lib, 2, 3);
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());

    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    let (root, root_whole) = db.index_dir(&lib).expect("the root is recorded");
    assert_eq!((root.audio_files, root.complete, root_whole), (1, true, true));
    let album = lib.join("Artist 01").join("Album");
    let (record, whole) = db.index_dir(&album).expect("the album is recorded");
    assert_eq!((record.audio_files, record.complete, whole), (3, true, true));
    let artists = [lib.join("Artist 00"), lib.join("Artist 01")];
    assert_eq!(db.indexed_subtrees(&artists).len(), 2);
}

/// Stopping part-way keeps the rows read so far; the next pass reads only the rest.
#[test]
fn a_stopped_pass_keeps_its_rows_and_the_next_one_resumes() {
    common::isolate();
    let (lib, db_path) = scratch("resume");
    let files = library_tree(&lib, 4, 5);
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let reads = Arc::new(AtomicUsize::new(0));
    let reader: IndexTagReader = {
        let (control, reads, real) = (control.clone(), reads.clone(), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            if reads.fetch_add(1, Ordering::SeqCst) + 1 == 7 {
                control.stop();
            }
            real(path, stamp)
        })
    };
    let path = db_path.clone();
    let cfg = IndexConfig {
        write_every: 3,
        ..config(vec![lib.clone()])
    };
    run_library_index(|| AnalysisDatabase::open_at(&path), cfg, &control, &progress, reader);
    assert_eq!(reads.load(Ordering::SeqCst), 7);
    assert_eq!(
        rows_under(&db_path, &lib).len(),
        7,
        "every row read before the stop is kept"
    );
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    let partial: Vec<_> = [lib.clone()]
        .into_iter()
        .chain((0..4).map(|a| lib.join(format!("Artist {a:02}"))))
        .collect();
    assert!(
        db.indexed_subtrees(&partial).len() < 4,
        "a folder stopped part-way is not complete"
    );

    let count = Arc::new(AtomicUsize::new(0));
    let next = one_pass(
        &db_path,
        vec![lib.clone()],
        counting_reader(count.clone(), Default::default()),
    );
    assert_eq!(count.load(Ordering::SeqCst), files.len() - 7);
    assert_eq!((next.read, next.unchanged), (files.len() - 7, 7));
    assert_eq!(rows_under(&db_path, &lib).len(), files.len());
}

// ---------- what a row holds ----------

/// An I/O error stores no row and leaves the folder, and every folder above it, incomplete.
#[test]
fn an_io_error_writes_no_row_and_leaves_the_folder_incomplete() {
    common::isolate();
    let (lib, db_path) = scratch("io_error");
    library_tree(&lib, 2, 3);
    let bad = lib.join("Artist 01").join("Album").join("01 Song.wav");
    let reader: IndexTagReader = {
        let (bad, real) = (bad.clone(), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            if path == bad {
                return Err(TagReadError::Io(std::io::Error::from_raw_os_error(5)));
            }
            real(path, stamp)
        })
    };
    let snap = one_pass(&db_path, vec![lib.clone()], reader);
    assert_eq!((snap.read, snap.failed), (6, 1));

    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(db.get_track(&bad).is_none(), "no row for a file that couldn't be read");
    let album = lib.join("Artist 01").join("Album");
    let (record, whole) = db.index_dir(&album).unwrap();
    assert!(!record.complete && !whole);
    let artists = [lib.join("Artist 00"), lib.join("Artist 01")];
    let indexed = db.indexed_subtrees(&artists);
    assert!(indexed.contains_key(&artists[0]) && !indexed.contains_key(&artists[1]));
    assert!(!db.index_dir(&lib).unwrap().1, "the root's subtree isn't whole either");
}

/// A tag read that panics (lofty on a malformed file) costs that file only (#146 review): the
/// pass goes on and finishes, the file counts as failed, and a later pass doesn't read it again
/// while its stamp is the same, nor lets it keep its folder incomplete.
#[test]
fn a_panicking_tag_read_costs_that_file_only() {
    common::isolate();
    let (lib, db_path) = scratch("panic");
    library_tree(&lib, 2, 3);
    let bad = lib.join("Artist 01").join("Album").join("01 Song.wav");
    let bad_reads = Arc::new(AtomicUsize::new(0));
    let reader: IndexTagReader = {
        let (bad, bad_reads, real) = (bad.clone(), bad_reads.clone(), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            if path == bad {
                bad_reads.fetch_add(1, Ordering::SeqCst);
                panic!("malformed file");
            }
            real(path, stamp)
        })
    };
    let first = one_pass(&db_path, vec![lib.clone()], reader.clone());
    assert_eq!((first.read, first.failed), (6, 1), "{first:?}");
    let album = lib.join("Artist 01").join("Album");
    {
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(db.get_track(&bad).is_none());
        assert!(!db.index_dir(&album).unwrap().1, "first failure: not whole yet");
    }

    let second = one_pass(&db_path, vec![lib.clone()], reader);
    assert_eq!((second.read, second.unchanged, second.failed), (0, 6, 1), "{second:?}");
    assert_eq!(bad_reads.load(Ordering::SeqCst), 1, "not read again for the same stamp");
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(trusted(&db, &album) && trusted(&db, &lib.join("Artist 01")) && trusted(&db, &lib));
}

/// A file that always fails (EACCES) keeps its folder incomplete for one pass only (#146
/// review): failing again with the same stamp, it no longer stops the folder, its top-level
/// folder and the root from being marked whole (a walk would only offer it untagged). It is still
/// tried every pass, and once it reads its row is written and the failure forgotten.
#[test]
fn a_file_that_always_fails_stops_blocking_its_folder() {
    common::isolate();
    let (lib, db_path) = scratch("always_fails");
    library_tree(&lib, 2, 3);
    let bad = lib.join("Artist 01").join("Album").join("01 Song.wav");
    let readable = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let reader: IndexTagReader = {
        let (bad, readable, attempts, real) = (bad.clone(), readable.clone(), attempts.clone(), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            if path == bad {
                attempts.fetch_add(1, Ordering::SeqCst);
                if !readable.load(Ordering::SeqCst) {
                    return Err(TagReadError::Io(std::io::Error::from_raw_os_error(13)));
                }
            }
            real(path, stamp)
        })
    };
    let album = lib.join("Artist 01").join("Album");
    one_pass(&db_path, vec![lib.clone()], reader.clone());
    {
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(!trusted(&db, &album) && !trusted(&db, &lib.join("Artist 01")) && !trusted(&db, &lib));
    }
    let second = one_pass(&db_path, vec![lib.clone()], reader.clone());
    assert_eq!(second.failed, 1, "{second:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "tried again");
    {
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(trusted(&db, &album) && trusted(&db, &lib.join("Artist 01")) && trusted(&db, &lib));
        assert!(db.get_track(&bad).is_none(), "still no row");
        assert_eq!(db.index_failures(std::slice::from_ref(&bad)).len(), 1);
    }

    readable.store(true, Ordering::SeqCst);
    let third = one_pass(&db_path, vec![lib.clone()], reader);
    assert_eq!((third.read, third.failed), (1, 0), "{third:?}");
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(db.get_track(&bad).is_some());
    assert!(
        db.index_failures(std::slice::from_ref(&bad)).is_empty(),
        "forgotten once read"
    );
}

/// An untagged file gets a row from its path, waiting for its acoustic pass; a BPM-tagged one
/// needs none.
#[test]
fn rows_take_the_path_heuristics_or_the_bpm_tag() {
    common::isolate();
    let (lib, db_path) = scratch("rows");
    let untagged = lib
        .join("Some Artist")
        .join("Some Album")
        .join("Some Artist - Some Song.wav");
    silent_wav(&untagged);
    let tagged = lib.join("Some Artist").join("Some Album").join("tagged.wav");
    tagged_wav(&tagged, "Tagged Title", "124");
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());

    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    let row = db.get_track(&untagged).expect("an untagged file gets a row");
    let (title, artist, album) = quickstream::metadata::parse_path_heuristics(&untagged);
    assert_eq!((row.title, row.artist, row.album), (title, artist, album));
    assert!(row.needs_acoustic, "no BPM tag: it waits for the acoustic pass");
    assert_eq!((row.bpm, row.energy), (None, None));

    let row = db.get_track(&tagged).expect("a tagged file gets a row");
    assert_eq!(row.title, "Tagged Title");
    assert_eq!(row.bpm, Some(124.0));
    assert!(!row.needs_acoustic, "a BPM tag needs no acoustic pass");
}

/// Hidden folders and files, recycle bins and symlinked folders are skipped, as the mix's walk
/// skips them. A symlinked audio file is indexed under its own path (its stat and tag read follow
/// the link), as the walk offers it; a link to nothing has no file to index.
#[cfg(unix)]
#[test]
fn hidden_folders_recycle_bins_and_symlinks_are_skipped() {
    common::isolate();
    let (lib, db_path) = scratch("skips");
    let outside = lib.parent().unwrap().join("outside");
    silent_wav(&outside.join("elsewhere.wav"));
    let kept = lib.join("Artist").join("kept.wav");
    silent_wav(&kept);
    for skipped in [
        lib.join(".hidden").join("a.wav"),
        lib.join("#recycle").join("b.wav"),
        lib.join("$RECYCLE.BIN").join("c.wav"),
        lib.join("Artist").join("._kept.wav"),
        lib.join("Artist").join(".d.wav"),
    ] {
        silent_wav(&skipped);
    }
    std::os::unix::fs::symlink(&outside, lib.join("Linked Folder")).unwrap();
    let link = lib.join("Artist").join("link.wav");
    std::os::unix::fs::symlink(outside.join("elsewhere.wav"), &link).unwrap();
    std::os::unix::fs::symlink(outside.join("gone.wav"), lib.join("Artist").join("dangling.wav")).unwrap();

    let snap = one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    assert_eq!((snap.read, snap.failed), (2, 0), "{snap:?}");
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    let paths: Vec<PathBuf> = db.tracks_under(&lib, 64).into_iter().map(|r| r.filepath).collect();
    assert_eq!(paths, vec![kept, link]);
    assert!(db.get_track(&outside.join("elsewhere.wav")).is_none());
    assert!(db.get_track(&lib.join(".hidden").join("a.wav")).is_none());
    assert!(db.get_track(&lib.join("#recycle").join("b.wav")).is_none());
    assert!(db.index_dir(&lib).unwrap().1, "the tree is whole");
}

/// A name that isn't UTF-8 can't be keyed in the index, so a folder holding such a subfolder or
/// audio file is recorded incomplete (the mix walks it, and finds the file); other names in it
/// are indexed. Where the file system refuses such names (APFS, HFS+) there is nothing to test.
#[cfg(unix)]
#[test]
fn a_name_that_is_not_utf8_leaves_its_folder_incomplete() {
    common::isolate();
    use std::os::unix::ffi::OsStrExt;
    let (lib, db_path) = scratch("not_utf8");
    library_tree(&lib, 2, 1);
    let odd = lib.join("Artist 00").join(std::ffi::OsStr::from_bytes(b"caf\xe9.wav"));
    if fs::write(&odd, b"").is_err() {
        return;
    }
    silent_wav(&odd);
    let quiet = lib
        .join("Artist 01")
        .join(std::ffi::OsStr::from_bytes(b"notes \xff.txt"));
    fs::write(&quiet, b"not audio").unwrap();
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    let (record, whole) = db.index_dir(&lib.join("Artist 00")).unwrap();
    assert!(!record.complete && !whole);
    assert!(!db.index_dir(&lib).unwrap().1);
    assert!(
        db.index_dir(&lib.join("Artist 01")).unwrap().1,
        "a name that isn't audio doesn't matter"
    );
}

// ---------- order ----------

/// A prioritised top-level folder is walked before the others.
#[test]
fn a_prioritised_folder_is_walked_first() {
    common::isolate();
    let (lib, db_path) = scratch("prioritise");
    library_tree(&lib, 12, 2);
    let order = Arc::new(Mutex::new(Vec::new()));
    let reader = counting_reader(Arc::new(AtomicUsize::new(0)), order.clone());
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let wanted = lib.join("Artist 07");
    control.prioritise(wanted.clone());
    let path = db_path.clone();
    let c = control.clone();
    let p = progress.clone();
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![lib]), &c, &p, reader)
    });
    wait_for("the pass", Duration::from_secs(20), || {
        progress.state() == IndexState::Done
    });
    control.stop();
    handle.join().unwrap();
    let order = order.lock().unwrap();
    // The root's own loose track comes first (it is read as the root is listed), then Artist 07.
    assert_eq!(order.len(), 25);
    assert!(order[0].ends_with("loose.wav"), "{order:?}");
    assert!(
        order[1].starts_with(&wanted) && order[2].starts_with(&wanted),
        "{order:?}"
    );
}

/// A prioritised folder deeper down is walked at once, and not again when the walk gets there.
#[test]
fn a_prioritised_folder_deeper_down_is_walked_once() {
    common::isolate();
    let (lib, db_path) = scratch("prioritise_deep");
    library_tree(&lib, 6, 3);
    let order = Arc::new(Mutex::new(Vec::new()));
    let reader = counting_reader(Arc::new(AtomicUsize::new(0)), order.clone());
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let wanted = lib.join("Artist 03").join("Album");
    control.prioritise(wanted.clone());
    let (path, c, p) = (db_path.clone(), control.clone(), progress.clone());
    let root = lib.clone();
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![root]), &c, &p, reader)
    });
    wait_for("the pass", Duration::from_secs(20), || {
        progress.state() == IndexState::Done
    });
    control.stop();
    handle.join().unwrap();
    let order = order.lock().unwrap();
    assert_eq!(order.len(), 19, "every file read once: {order:?}");
    assert!(order[1..4].iter().all(|p| p.starts_with(&wanted)), "{order:?}");
    let unique: HashSet<_> = order.iter().collect();
    assert_eq!(unique.len(), order.len());
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(db.index_dir(&lib).unwrap().1, "the whole tree is indexed");
}

/// A prioritised folder is walked only when the listing would reach it (#146 review): its path
/// is normalized (`..` resolved) and must lie under a root, and no folder below the root on the
/// way may be one the walk skips (hidden, `#recycle`) or a symlink. A hint that fails is
/// ignored and the pass goes on as usual; one that normalizes into a root is taken.
#[cfg(unix)]
#[test]
fn a_prioritised_folder_is_walked_only_where_the_listing_goes() {
    common::isolate();
    let (lib, db_path) = scratch("prioritise_checks");
    library_tree(&lib, 8, 1);
    let outside = lib.parent().unwrap().join("outside");
    silent_wav(&outside.join("Album").join("elsewhere.wav"));
    silent_wav(&lib.join(".hidden").join("Album").join("hidden.wav"));
    silent_wav(&lib.join("#recycle").join("Album").join("binned.wav"));
    std::os::unix::fs::symlink(&outside, lib.join("Linked")).unwrap();
    std::os::unix::fs::symlink(&outside, lib.join("Artist 01").join("Linked")).unwrap();

    let hints = [
        lib.join(".hidden").join("Album"),
        lib.join("#recycle").join("Album"),
        lib.join("Linked").join("Album"),
        lib.join("Artist 01").join("Linked").join("Album"),
        lib.join("Artist 00")
            .join("..")
            .join("..")
            .join("outside")
            .join("Album"),
        outside.join("Album"),
    ];
    for (i, hint) in hints.into_iter().enumerate() {
        // A database of its own for each hint, so each walks the tree afresh.
        let db_path = db_path.with_file_name(format!("hint{i}.db"));
        let order = Arc::new(Mutex::new(Vec::new()));
        let reader = counting_reader(Arc::new(AtomicUsize::new(0)), order.clone());
        let run = {
            let control = Arc::new(IndexControl::new());
            control.prioritise(hint.clone());
            let progress = Arc::new(IndexProgress::new());
            let (c, p, path, root) = (control.clone(), progress.clone(), db_path.clone(), lib.clone());
            let handle = std::thread::spawn(move || {
                run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![root]), &c, &p, reader)
            });
            Running {
                control,
                progress,
                handle,
            }
        };
        run.wait_state(IndexState::Done);
        run.finish();
        let order = order.lock().unwrap();
        assert!(
            order.iter().all(|p| p.starts_with(&lib)
                && !p.starts_with(lib.join(".hidden"))
                && !p.starts_with(lib.join("#recycle"))
                && !p.to_string_lossy().contains("Linked")),
            "{} was walked: {order:?}",
            hint.display()
        );
        assert_eq!(order.len(), 9, "the pass went on as usual: {order:?}");
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(db.get_track(&outside.join("Album").join("elsewhere.wav")).is_none());
        assert!(db.tracks_under(&lib.join("Linked"), 64).is_empty());
    }

    // `..` resolved into the root: Artist 06 first, keyed by its own path.
    let order = Arc::new(Mutex::new(Vec::new()));
    let reader = counting_reader(Arc::new(AtomicUsize::new(0)), order.clone());
    let control = Arc::new(IndexControl::new());
    control.prioritise(lib.join("Artist 03").join("..").join("Artist 06"));
    let progress = Arc::new(IndexProgress::new());
    let (c, p, path, root) = (control.clone(), progress.clone(), db_path.clone(), lib.clone());
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![root]), &c, &p, reader)
    });
    wait_for("Done", Duration::from_secs(20), || progress.state() == IndexState::Done);
    control.stop();
    handle.join().unwrap();
    let order = order.lock().unwrap();
    assert!(order[1].starts_with(lib.join("Artist 06")), "{order:?}");
    assert!(order.iter().all(|p| !p.to_string_lossy().contains("..")), "{order:?}");
}

/// Roots are resolved on the indexer's thread before it walks them (#146 review): a Music Home
/// that is a symlink to another one is walked once, and one that resolves to `/` (or the home
/// folder, or above it) not at all.
#[cfg(unix)]
#[test]
fn a_symlinked_root_is_resolved_before_it_is_walked() {
    common::isolate();
    let (lib, db_path) = scratch("root_links");
    let real = lib.join("Real");
    let files = library_tree(&real, 2, 2);
    let link = lib.join("Link To Real");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let slash = lib.join("Slash");
    std::os::unix::fs::symlink("/", &slash).unwrap();

    // Anything outside `real` would mean the boot disk is being walked: stop at once.
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let count = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let reader: IndexTagReader = {
        let (control, real_dir, inner) = (
            control.clone(),
            real.clone(),
            counting_reader(count.clone(), order.clone()),
        );
        Arc::new(move |path: &Path, stamp| {
            if !path.starts_with(&real_dir) {
                control.stop();
            }
            inner(path, stamp)
        })
    };
    let (c, p, path) = (control.clone(), progress.clone(), db_path.clone());
    let roots = vec![slash.clone(), real.clone(), link.clone()];
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), config(roots), &c, &p, reader)
    });
    wait_for("Done", Duration::from_secs(20), || {
        progress.state() == IndexState::Done || control.is_stopped()
    });
    control.stop();
    handle.join().unwrap();
    let order = order.lock().unwrap();
    assert!(order.iter().all(|p| p.starts_with(&real)), "{order:?}");
    assert_eq!(count.load(Ordering::SeqCst), files.len(), "each file read once");
    assert_eq!(progress.snapshot().top_total, 2, "{:?}", progress.snapshot());

    // Only links: nothing to walk.
    let progress = IndexProgress::new();
    run_library_index(
        || panic!("no database is opened without a folder"),
        config(vec![slash]),
        &IndexControl::new(),
        &progress,
        default_tag_reader(),
    );
    assert_eq!(progress.state(), IndexState::NoFolders);
}

// ---------- pacing ----------

/// While paused it reads nothing and says so; unpaused it goes on.
#[test]
fn a_paused_indexer_reads_nothing_until_resumed() {
    common::isolate();
    let (lib, db_path) = scratch("pause");
    library_tree(&lib, 2, 2);
    let count = Arc::new(AtomicUsize::new(0));
    let reader = counting_reader(count.clone(), Default::default());
    let control = Arc::new(IndexControl::new());
    control.set_paused(true);
    let progress = Arc::new(IndexProgress::new());
    let (path, c, p) = (db_path.clone(), control.clone(), progress.clone());
    let root = lib.clone();
    let handle = std::thread::spawn(move || {
        run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![root]), &c, &p, reader)
    });
    wait_for("Paused", Duration::from_secs(10), || {
        progress.state() == IndexState::Paused
    });
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    control.set_paused(false);
    wait_for("Done", Duration::from_secs(20), || progress.state() == IndexState::Done);
    assert_eq!(count.load(Ordering::SeqCst), 5);
    control.stop();
    handle.join().unwrap();
}

/// A share that is slow but working (2-3 s tag reads over Wi-Fi) keeps being read at nearly its
/// own speed (#146 review): the indexer backs off only after several slow operations in a row,
/// for as long as the last one took, so every read slow costs at most a third more. One slow
/// read among fast ones costs no wait at all.
#[test]
fn a_slow_share_is_read_at_nearly_its_own_speed() {
    common::isolate();
    let (lib, db_path) = scratch("slow_share");
    // Six tracks in one folder: their reads come one after another.
    for t in 0..6 {
        silent_wav(&lib.join(format!("{t:02} Song.wav")));
    }
    let read_time = Duration::from_millis(250);
    let slow: IndexTagReader = {
        let real = default_tag_reader();
        Arc::new(move |path: &Path, stamp| {
            std::thread::sleep(read_time);
            real(path, stamp)
        })
    };
    let cfg = IndexConfig {
        slow_op: Duration::from_millis(200),
        slow_streak: 3,
        backoff: Duration::from_secs(60),
        ..config(vec![lib.clone()])
    };
    let start = Instant::now();
    let run = spawn(&db_path, cfg.clone(), slow);
    run.wait_state(IndexState::Done);
    let took = start.elapsed();
    let snap = run.finish();
    assert_eq!((snap.read, snap.backoffs), (6, 2), "{snap:?}");
    // Six reads of 250 ms and two waits of about as long; a fixed backoff would take minutes.
    assert!(took >= read_time * 8, "{took:?}");
    assert!(took < read_time * 16, "{took:?}");

    // One slow read among fast ones: no wait.
    let (lib2, db2) = scratch("one_slow");
    library_tree(&lib2, 1, 5);
    let one_slow: IndexTagReader = {
        let real = default_tag_reader();
        Arc::new(move |path: &Path, stamp| {
            if path.ends_with("02 Song.wav") {
                std::thread::sleep(read_time);
            }
            real(path, stamp)
        })
    };
    let snap = one_pass_with(
        &db2,
        IndexConfig {
            roots: vec![lib2],
            ..cfg
        },
        one_slow,
    );
    assert_eq!((snap.read, snap.backoffs), (6, 0), "{snap:?}");
}

/// One operation that takes as long as the backoff cap (a stalled share) backs off at once, for
/// at most the cap; stop ends the backoff at once.
#[test]
fn a_stall_backs_off_at_once_and_stop_ends_the_backoff() {
    common::isolate();
    let (lib, db_path) = scratch("backoff_stop");
    library_tree(&lib, 1, 2);
    let stall: IndexTagReader = {
        let real = default_tag_reader();
        Arc::new(move |path: &Path, stamp| {
            if path.ends_with("00 Song.wav") {
                std::thread::sleep(Duration::from_millis(1500));
            }
            real(path, stamp)
        })
    };
    let cfg = IndexConfig {
        slow_op: Duration::from_millis(100),
        backoff: Duration::from_millis(1200),
        ..config(vec![lib])
    };
    let run = spawn(&db_path, cfg, stall);
    wait_for("the backoff", Duration::from_secs(10), || {
        run.progress.backoffs.load(Ordering::SeqCst) == 1
    });
    let stop = Instant::now();
    run.finish();
    assert!(stop.elapsed() < Duration::from_millis(600), "{:?}", stop.elapsed());
}

/// A reader stuck on a hung share neither keeps quitting waiting (the thread is detached after
/// the registry's deadline) nor holds a lock on the database.
#[test]
fn a_hung_reader_blocks_neither_shutdown_nor_the_database() {
    common::isolate();
    let (lib, db_path) = scratch("hung");
    library_tree(&lib, 2, 2);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let reader: IndexTagReader = {
        let entered_tx = Mutex::new(entered_tx);
        Arc::new(move |_path: &Path, _stamp| {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
            Err(TagReadError::Io(std::io::Error::from_raw_os_error(60)))
        })
    };
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let mut workers = WorkerRegistry::new();
    {
        let (path, c, p, root) = (db_path.clone(), control.clone(), progress.clone(), lib.clone());
        workers.spawn("library-index", move || {
            run_library_index(|| AnalysisDatabase::open_at(&path), config(vec![root]), &c, &p, reader)
        });
    }
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the reader is called");

    // Another connection writes at once: no transaction is open across the read.
    let other = AnalysisDatabase::open_at(&db_path).unwrap();
    let start = Instant::now();
    other
        .upsert_track(&TrackFeatures {
            filepath: lib.join("elsewhere.wav"),
            ..Default::default()
        })
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());

    control.stop();
    let start = Instant::now();
    assert_eq!(
        workers.join_all(Duration::from_millis(100)),
        1,
        "the stuck thread is detached"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(release_tx);
}

// ---------- states ----------

#[test]
fn a_database_that_wont_open_reports_unavailable() {
    common::isolate();
    let (lib, _) = scratch("unavailable");
    library_tree(&lib, 1, 1);
    let control = IndexControl::new();
    let progress = IndexProgress::new();
    run_library_index(
        || Err(anyhow::anyhow!("in memory")),
        config(vec![lib]),
        &control,
        &progress,
        default_tag_reader(),
    );
    assert_eq!(progress.state(), IndexState::Unavailable);
}

#[test]
fn no_roots_reports_no_folders_at_once() {
    common::isolate();
    let control = IndexControl::new();
    let progress = IndexProgress::new();
    let start = Instant::now();
    run_library_index(
        || panic!("no database is opened without a folder"),
        IndexConfig::new(Vec::new()),
        &control,
        &progress,
        default_tag_reader(),
    );
    assert_eq!(progress.state(), IndexState::NoFolders);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "no start delay without a folder"
    );
}

/// Stop during the start delay returns at once, without opening the database.
#[test]
fn stop_during_the_start_delay_returns_without_opening_the_database() {
    common::isolate();
    let (lib, _) = scratch("delay");
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let cfg = IndexConfig {
        start_delay: Duration::from_secs(60),
        ..IndexConfig::new(vec![lib])
    };
    let (c, p) = (control.clone(), progress.clone());
    let handle = std::thread::spawn(move || {
        run_library_index(|| panic!("opened during the delay"), cfg, &c, &p, default_tag_reader())
    });
    wait_for("Waiting", Duration::from_secs(5), || {
        progress.state() == IndexState::Waiting
    });
    let start = Instant::now();
    control.stop();
    handle.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// The folder total is known once the roots are listed, and counts every root's top folders.
#[test]
fn progress_counts_top_level_folders_of_every_root() {
    common::isolate();
    let (lib, db_path) = scratch("roots");
    let a = lib.join("A");
    let b = lib.join("B");
    library_tree(&a, 3, 1);
    library_tree(&b, 2, 1);
    let snap = one_pass(&db_path, vec![a.clone(), b.clone()], default_tag_reader());
    assert_eq!((snap.top_done, snap.top_total, snap.read), (5, 5, 7));
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(db.index_dir(&a).unwrap().1 && db.index_dir(&b).unwrap().1);
}

/// The folder total shows only once every root has been listed (#146 review): while the second
/// root is being listed it is still 0, never the first root's count alone.
#[test]
fn the_folder_total_shows_once_every_root_is_listed() {
    common::isolate();
    let (lib, db_path) = scratch("total");
    let a = lib.join("A");
    let b = lib.join("B");
    library_tree(&a, 3, 1);
    library_tree(&b, 2, 1);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let reader: IndexTagReader = {
        let (gate, entered_tx, release_rx, real) = (
            b.join("loose.wav"),
            Mutex::new(entered_tx),
            Mutex::new(release_rx),
            default_tag_reader(),
        );
        Arc::new(move |path: &Path, stamp| {
            if path == gate {
                let _ = entered_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            }
            real(path, stamp)
        })
    };
    let run = spawn(&db_path, config(vec![a, b]), reader);
    entered_rx.recv_timeout(Duration::from_secs(20)).expect("B is listed");
    assert_eq!(
        run.progress.snapshot().top_total,
        0,
        "A's folders alone aren't the total"
    );
    drop(release_tx);
    run.wait_state(IndexState::Done);
    assert_eq!(run.finish().top_total, 5);
}

// ---------- the indexer in the app (ui::library_index) ----------

use quickstream::ui::library_index::{index_roots, LibraryIndexSource};
use quickstream::ui::AppUI;

/// The folders indexed: the startup home first, then the Music Homes; never `/`, the home folder
/// or a folder above it, nor a relative path; each once (`..` resolved), none inside another.
#[test]
fn index_roots_are_the_startup_home_and_music_homes_without_overlap() {
    common::isolate();
    let home = Path::new("/Users/someone");
    let homes: Vec<String> = [
        "/Volumes/Music/Jazz",
        "/Volumes/Music",
        "/Volumes/Other/../Music",
        "/",
        "/Users",
        "/Users/someone",
        "relative/folder",
        "/Users/someone/Music",
        "/Volumes/Music2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        index_roots(Some("/Users/someone/Music/Live"), &homes, Some(home)),
        [
            PathBuf::from("/Volumes/Music"),
            PathBuf::from("/Users/someone/Music"),
            PathBuf::from("/Volumes/Music2"),
        ],
        "the startup home lies inside a Music Home"
    );
    assert_eq!(
        index_roots(Some("/Data/Audio"), &[], Some(home)),
        [PathBuf::from("/Data/Audio")]
    );
    assert!(index_roots(None, &[], Some(home)).is_empty());
    assert!(index_roots(Some("/Users/someone"), &[], Some(home)).is_empty());
}

/// An app whose indexer runs on `db_path` with `reader`, starting at once, over `lib` (its only
/// Music Home).
fn index_app(lib: &Path, db_path: &Path, reader: IndexTagReader) -> AppUI {
    let mut settings = quickstream::settings::SettingsManager::new();
    settings.settings.web_server_enabled = false;
    // On whatever a test that toggled it saved to the shared config file.
    settings.settings.library_index = true;
    let mut app = AppUI::new(Some(lib.to_path_buf()), settings);
    // Reading the folder the app opens at holds the indexer: let it finish first.
    common::wait_for_dir_scan(&mut app);
    app.bookmarks = vec![lib.to_string_lossy().into_owned()];
    let path = db_path.to_path_buf();
    app.set_library_index_source(LibraryIndexSource {
        open_db: Arc::new(move || AnalysisDatabase::open_at(&path)),
        reader,
        config: IndexConfig {
            start_delay: Duration::ZERO,
            ..IndexConfig::default()
        },
    });
    app
}

fn poll_until(app: &mut AppUI, what: &str, mut done: impl FnMut(&mut AppUI) -> bool) {
    let start = Instant::now();
    while !done(app) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        app.poll_library_index();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// While a mix gathers (or a folder is read, or a search scans) the indexer holds; then it goes on.
#[test]
fn the_app_pauses_the_indexer_while_it_reads_the_library_itself() {
    common::isolate();
    let (lib, db_path) = scratch("app_pause");
    library_tree(&lib, 3, 2);
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    let first = Arc::new(AtomicUsize::new(0));
    let reader: IndexTagReader = {
        let (first, real) = (first.clone(), default_tag_reader());
        // The first read waits for the test, so the pause lands mid-pass.
        Arc::new(move |path: &Path, stamp| {
            if first.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = gate_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            }
            real(path, stamp)
        })
    };
    let mut app = index_app(&lib, &db_path, reader);
    app.start_library_index();
    poll_until(&mut app, "the first read", |_| first.load(Ordering::SeqCst) == 1);
    app.is_autopilot_scanning = true;
    app.poll_library_index();
    drop(gate_tx);
    let progress = app.library_index_progress();
    poll_until(&mut app, "Paused", |_| progress.state() == IndexState::Paused);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(first.load(Ordering::SeqCst), 1, "no read while paused");
    app.is_autopilot_scanning = false;
    poll_until(&mut app, "Done", |_| progress.state() == IndexState::Done);
    assert_eq!(progress.snapshot().read, 7);
}

/// A Music Home added while it runs restarts it over the new folders.
#[test]
fn a_new_music_home_restarts_the_indexer_over_it() {
    common::isolate();
    let (lib, db_path) = scratch("app_roots");
    let a = lib.join("A");
    let b = lib.join("B");
    library_tree(&a, 2, 1);
    library_tree(&b, 2, 1);
    let mut app = index_app(&a, &db_path, default_tag_reader());
    app.start_library_index();
    let progress = app.library_index_progress();
    poll_until(&mut app, "the first pass", |_| progress.state() == IndexState::Done);
    assert_eq!(app.library_index_starts(), 1);
    assert!(rows_under(&db_path, &b).is_empty());

    app.bookmarks.push(b.to_string_lossy().into_owned());
    poll_until(&mut app, "the restart", |app| app.library_index_starts() == 2);
    poll_until(&mut app, "the second pass", |_| progress.state() == IndexState::Done);
    assert_eq!(app.index_roots(), [a.clone(), b.clone()]);
    assert_eq!(rows_under(&db_path, &b).len(), 3, "the new home is indexed");
    assert_eq!(progress.snapshot().unchanged, 3, "the old one isn't read again");
}

/// With no folder to index it says so, and a Music Home added later starts it.
#[test]
fn no_folders_until_a_music_home_is_added() {
    common::isolate();
    let (lib, db_path) = scratch("app_none");
    library_tree(&lib, 1, 1);
    let mut app = index_app(&lib, &db_path, default_tag_reader());
    app.bookmarks.clear();
    app.start_library_index();
    let progress = app.library_index_progress();
    poll_until(&mut app, "NoFolders", |_| progress.state() == IndexState::NoFolders);
    app.bookmarks.push(lib.to_string_lossy().into_owned());
    poll_until(&mut app, "Done", |_| progress.state() == IndexState::Done);
    assert_eq!(app.library_index_starts(), 2);
}

/// Turned off, it stops and the setting row reads OFF; it never starts while off.
#[test]
fn the_setting_off_stops_the_indexer_and_keeps_it_stopped() {
    common::isolate();
    let (lib, db_path) = scratch("app_off");
    library_tree(&lib, 1, 1);
    let mut app = index_app(&lib, &db_path, default_tag_reader());
    app.settings_mgr.settings.library_index = false;
    app.start_library_index();
    assert_eq!(app.library_index_starts(), 0);
    app.settings_mgr.settings.library_index = true;
    app.start_library_index();
    assert!(app.library_index_running());
    app.settings_cursor = quickstream::settings::setting::LIBRARY_INDEX;
    app.cycle_and_apply_setting(true);
    assert!(!app.library_index_running());
    app.bookmarks.push(lib.join("Artist 00").to_string_lossy().into_owned());
    for _ in 0..20 {
        app.poll_library_index();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        app.library_index_starts(),
        1,
        "a changed home doesn't start it while off"
    );
}

/// Turned off and on again while the old thread is still busy (a read on a slow share), the
/// restart waits for it, and the row reads WAITING meanwhile, not OFF (#146 review).
#[test]
fn turned_on_again_while_the_old_thread_finishes_it_reads_waiting() {
    common::isolate();
    let (lib, db_path) = scratch("app_waiting");
    library_tree(&lib, 1, 1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered = Arc::new(AtomicUsize::new(0));
    let reader: IndexTagReader = {
        let (entered, release_rx, real) = (entered.clone(), Mutex::new(release_rx), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            if entered.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            }
            real(path, stamp)
        })
    };
    let mut app = index_app(&lib, &db_path, reader);
    app.start_library_index();
    poll_until(&mut app, "the first read", |_| entered.load(Ordering::SeqCst) == 1);
    app.settings_cursor = quickstream::settings::setting::LIBRARY_INDEX;
    app.cycle_and_apply_setting(true);
    let progress = app.library_index_progress();
    assert_eq!(progress.state(), IndexState::Off);
    app.cycle_and_apply_setting(true);
    assert!(app.settings_mgr.settings.library_index);
    assert_eq!(app.library_index_starts(), 1, "the old thread is still busy");
    assert_eq!(progress.state(), IndexState::Waiting);
    drop(release_tx);
    poll_until(&mut app, "the restart", |app| app.library_index_starts() == 2);
    poll_until(&mut app, "Done", |_| progress.state() == IndexState::Done);
}

/// The folders to index are worked out again only when the Music Homes or the startup home
/// change, not on every tick (#146 review).
#[test]
fn the_index_roots_are_worked_out_only_when_their_folders_change() {
    common::isolate();
    let (lib, db_path) = scratch("app_roots_cache");
    library_tree(&lib, 1, 1);
    let mut app = index_app(&lib, &db_path, default_tag_reader());
    app.start_library_index();
    let builds = app.library_index_root_builds();
    for _ in 0..50 {
        app.poll_library_index();
    }
    assert_eq!(app.library_index_root_builds(), builds, "no work on an unchanged tick");
    app.bookmarks.push(lib.join("Artist 00").to_string_lossy().into_owned());
    app.poll_library_index();
    app.poll_library_index();
    assert_eq!(app.library_index_root_builds(), builds + 1);
    app.settings_mgr.settings.home_dir = Some("/Data/Other".to_string());
    app.poll_library_index();
    assert_eq!(app.library_index_root_builds(), builds + 2);
    assert_eq!(app.index_roots(), [PathBuf::from("/Data/Other"), lib.clone()]);
}

/// A mix started under an index root makes the indexer walk the mix's folder next.
#[test]
fn a_mix_folder_is_indexed_first() {
    common::isolate();
    let (lib, db_path) = scratch("app_prioritise");
    library_tree(&lib, 10, 2);
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    let order = Arc::new(Mutex::new(Vec::new()));
    let reader: IndexTagReader = {
        let (order, real) = (order.clone(), default_tag_reader());
        // The first read (the root's loose track) waits until the mix has started.
        Arc::new(move |path: &Path, stamp| {
            let first = {
                let mut order = order.lock().unwrap();
                order.push(path.to_path_buf());
                order.len() == 1
            };
            if first {
                let _ = gate_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            }
            real(path, stamp)
        })
    };
    let mut app = index_app(&lib, &db_path, reader);
    app.start_library_index();
    poll_until(&mut app, "the first read", |_| order.lock().unwrap().len() == 1);

    let wanted = lib.join("Artist 06");
    app.start_autopilot(wanted.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(20));
    drop(gate_tx);
    let progress = app.library_index_progress();
    poll_until(&mut app, "Done", |_| progress.state() == IndexState::Done);
    let order = order.lock().unwrap();
    assert_eq!(order.len(), 21);
    assert!(
        order[1].starts_with(&wanted) && order[2].starts_with(&wanted),
        "{order:?}"
    );
}

/// Whether a mix trusts `dir`'s subtree mark: recorded, and for the folder's mtime as it is now.
fn trusted(db: &AnalysisDatabase, dir: &Path) -> bool {
    let marks = db.indexed_subtrees(&[dir.to_path_buf()]);
    marks.get(dir).is_some_and(|&mtime| folder_mtime(dir) == Some(mtime))
}

/// A folder that gained an album since the last pass isn't trusted until its subtree has been
/// walked again: stopped inside the new album, the artist folder's mark is still the one for its
/// old mtime, so a mix walks it and finds the album. Its album walked whole, and the root (whose
/// own mtime didn't change), keep theirs. Once a pass has walked it whole it is trusted again.
#[test]
fn a_rewalked_folder_is_not_whole_until_its_subtree_is_walked_again() {
    common::isolate();
    let (lib, db_path) = scratch("rewalk");
    library_tree(&lib, 2, 2);
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    let artist = lib.join("Artist 00");
    {
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(trusted(&db, &lib) && trusted(&db, &artist));
    }

    let new_album = artist.join("NewAlbum");
    silent_wav(&new_album.join("01 New.wav"));
    silent_wav(&new_album.join("02 New.wav"));

    // The second pass reads only the new album's files: stop at its first one.
    let control = Arc::new(IndexControl::new());
    let progress = Arc::new(IndexProgress::new());
    let reader: IndexTagReader = {
        let (control, real) = (control.clone(), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            control.stop();
            real(path, stamp)
        })
    };
    let path = db_path.clone();
    run_library_index(
        || AnalysisDatabase::open_at(&path),
        config(vec![lib.clone()]),
        &control,
        &progress,
        reader,
    );

    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(
        !trusted(&db, &artist),
        "the artist folder was stopped inside its new album"
    );
    assert!(
        trusted(&db, &artist.join("Album")),
        "the album walked whole keeps its mark"
    );
    assert!(trusted(&db, &lib), "the root's own mtime didn't change");

    one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    assert!(trusted(&db, &artist), "walked whole again");
}

/// A pass over a library that hasn't changed takes no mark away while it runs (#146 review):
/// half-way through (a touched file's tags being read), the root and every top-level folder are
/// still trusted, including the one being walked.
#[test]
fn marks_stay_while_a_pass_walks_an_unchanged_library() {
    common::isolate();
    let (lib, db_path) = scratch("marks_stay");
    let files = library_tree(&lib, 3, 2);
    one_pass(&db_path, vec![lib.clone()], default_tag_reader());
    let tops: Vec<PathBuf> = (0..3).map(|a| lib.join(format!("Artist {a:02}"))).collect();

    // A file's content changes in place (a retag): its folder's mtime stays.
    let touched = files.iter().find(|p| p.starts_with(&tops[1])).unwrap().clone();
    let file = fs::File::options().write(true).open(&touched).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(120)).unwrap();
    drop(file);

    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let reader: IndexTagReader = {
        let (entered_tx, release_rx, real) = (Mutex::new(entered_tx), Mutex::new(release_rx), default_tag_reader());
        Arc::new(move |path: &Path, stamp| {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            real(path, stamp)
        })
    };
    let run = spawn(&db_path, config(vec![lib.clone()]), reader);
    entered_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the touched file is read");
    {
        let db = AnalysisDatabase::open_at(&db_path).unwrap();
        assert!(trusted(&db, &lib), "the root keeps its mark during the pass");
        for top in &tops {
            assert!(trusted(&db, top), "{} keeps its mark", top.display());
        }
    }
    drop(release_tx);
    run.wait_state(IndexState::Done);
    let snap = run.finish();
    assert_eq!((snap.read, snap.unchanged), (1, files.len() - 1));
    let db = AnalysisDatabase::open_at(&db_path).unwrap();
    assert!(trusted(&db, &lib) && tops.iter().all(|t| trusted(&db, t)));
}
