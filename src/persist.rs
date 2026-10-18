//! Crash-safe persistence for Quickstream's own config and cache files (never library files).
//!
//! - [`atomic_write`] replaces a file by writing `<file>.<pid>.tmp` in the same directory,
//!   fsyncing it and renaming it over the target, so a crash leaves either the old or the new
//!   content, never a truncated file.
//! - [`load_json_or_backup`] copies a file that fails to parse to `<file>.bak` before the caller
//!   falls back to defaults, so the next save can't silently destroy the user's settings.
//! - [`create_new_file`] writes a new file the same way but never replaces one already there.
//! - [`PersistenceWorker`] performs those writes, and creates, deletes and renames of the same
//!   files (playlists), on one background thread so the UI thread never waits on disk. It
//!   applies them in the order they were queued, coalesces rapid saves and flushes on drop. A
//!   create or rename queued with a [`Reporter`] reports back on the caller's channel when it is
//!   done: [`PersistReport::Failed`] when it failed, [`PersistReport::Done`] otherwise.

use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Temp file used while replacing `path`: same directory (so the rename stays on one
/// filesystem), suffixed with the process id so two running instances never share one.
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Replace `path` with `bytes` atomically: write a temp file beside it, fsync, rename.
/// On error the original file is left untouched.
///
/// Callers writing the same path from several threads must serialize those calls (the temp
/// path is per process, not per call).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        if tmp.is_file() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
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

/// Keep a copy of a file that failed to parse and return where it went.
///
/// The first copy goes to `<file>.bak`. An existing backup is never overwritten: it only ever
/// holds content we refused to load, which may be the user's only copy of a hand-edited
/// config, so a later corrupt file goes to `<file>.bak.<timestamp>` instead. If an existing
/// backup already holds exactly these bytes (the same broken file loaded on every launch)
/// no new copy is made.
pub fn backup_corrupt_file(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let bak_prefix = format!("{}.bak", name);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == bak_prefix || fname.starts_with(&format!("{}.", bak_prefix)) {
                if fname.ends_with(".tmp") {
                    continue;
                }
                if fs::read(entry.path()).map(|b| b == bytes).unwrap_or(false) {
                    return Ok(entry.path());
                }
            }
        }
    }

    let mut target = dir.join(&bak_prefix);
    if target.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        target = dir.join(format!("{}.{}", bak_prefix, stamp));
        let mut n = 1;
        while target.exists() {
            target = dir.join(format!("{}.{}-{}", bak_prefix, stamp, n));
            n += 1;
        }
    }
    atomic_write(&target, bytes)?;
    Ok(target)
}

/// Read and parse the JSON file at `path`.
///
/// Returns `None` when the file is missing, unreadable, empty or fails to parse. A file that
/// fails to parse is first copied aside with [`backup_corrupt_file`] and the problem is logged,
/// so falling back to defaults (and saving them later) never loses the user's data. `what`
/// names the file in the log ("settings", "bookmarks").
pub fn load_json_or_backup<T: DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            crate::logger::log_raw(
                "WARN",
                "persist",
                &format!("could not read {} file '{}': {}", what, path.display(), e),
            );
            return None;
        }
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        crate::logger::log_raw(
            "WARN",
            "persist",
            &format!("{} file '{}' is empty; using defaults", what, path.display()),
        );
        return None;
    }
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            let saved = match backup_corrupt_file(path, &bytes) {
                Ok(bak) => format!("copied to '{}'", bak.display()),
                Err(be) => format!("backup FAILED: {}", be),
            };
            crate::logger::log_raw(
                "WARN",
                "persist",
                &format!(
                    "{} file '{}' failed to parse ({}); {}; using defaults",
                    what,
                    path.display(),
                    e,
                    saved
                ),
            );
            None
        }
    }
}

/// Write `bytes` to a new file at `path`, never replacing anything already there: fails with
/// `AlreadyExists` when `path` exists, including under another case or Unicode form on a disk
/// that ignores those. Atomic like [`atomic_write`]: the bytes go to a temp file that is fsynced
/// and then hard-linked to `path`, and a hard link fails rather than replace. A disk without hard
/// links falls back to `create_new`, which still never replaces.
pub fn create_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&tmp, path) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => create_new_in_place(path, bytes),
            linked => linked,
        }
    })();
    if tmp.is_file() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_parent_dir(path);
    Ok(())
}

/// [`create_new_file`] without the temp file, for disks that can't hard-link. If writing fails
/// after the file was created, the half-written file (ours) is removed again.
fn create_new_in_place(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if written.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    written
}

/// Move `from` to `to` with the operating system's rename, but never over another file. A file
/// already at `to` is only allowed when it is `from` itself: a case-only rename
/// (`road trip.m3u8` to `Road Trip.m3u8`) on a case-insensitive disk. The check and the rename
/// are two steps, so this guards against a file the caller didn't know about, not a race.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if let Ok(target) = fs::symlink_metadata(to) {
        let same = fs::symlink_metadata(from).is_ok_and(|source| same_file(&source, &target, from, to));
        if !same {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a file with that name already exists",
            ));
        }
    }
    fs::rename(from, to)
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata, _: &Path, _: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata, a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Writes one file's bytes; the worker's default is [`atomic_write`].
pub type Writer = std::sync::Arc<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;

/// A create or rename the writer could not do, sent to the [`Reporter`] it was queued with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistFailure {
    /// Nothing was written to `path`, and whatever was already there is untouched.
    /// `kind` is `AlreadyExists` when a file was in the way.
    Create {
        tag: u64,
        path: PathBuf,
        kind: io::ErrorKind,
        error: String,
    },
    /// The file is still at `from`; nothing was moved to `to`.
    Rename {
        tag: u64,
        from: PathBuf,
        to: PathBuf,
        kind: io::ErrorKind,
        error: String,
    },
}

/// What a create or rename queued with a [`Reporter`] came to. Exactly one report is sent per
/// operation, so the caller knows which of its operations the writer has not finished yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistReport {
    /// The operation tagged `tag` is finished and nothing about it needs undoing: it worked, its
    /// source was already gone (nothing moved, nothing in the way), or its source was a path
    /// whose own create or rename failed, which that operation's report already covers.
    Done { tag: u64 },
    /// The operation failed; see [`PersistFailure`].
    Failed(PersistFailure),
}

impl PersistReport {
    pub fn tag(&self) -> u64 {
        match self {
            PersistReport::Done { tag }
            | PersistReport::Failed(PersistFailure::Create { tag, .. })
            | PersistReport::Failed(PersistFailure::Rename { tag, .. }) => *tag,
        }
    }
}

/// Where a create or rename reports how it went: the caller's `tag` for the operation (the
/// playlist store numbers each create and rename) and a channel the caller polls, so the UI
/// thread learns the outcome without waiting on the disk.
#[derive(Debug, Clone)]
pub struct Reporter {
    tag: u64,
    tx: mpsc::Sender<PersistReport>,
}

impl Reporter {
    pub fn new(tag: u64, tx: mpsc::Sender<PersistReport>) -> Self {
        Self { tag, tx }
    }

    pub fn tag(&self) -> u64 {
        self.tag
    }

    /// A receiver that was dropped (its store replaced) has nobody left to tell.
    fn send(&self, failure: PersistFailure) {
        let _ = self.tx.send(PersistReport::Failed(failure));
    }

    fn done(&self) {
        let _ = self.tx.send(PersistReport::Done { tag: self.tag });
    }
}

/// Paths whose file is not where the caller believes, after a create or rename failed. Each
/// maps the path the caller keeps writing to onto the file that really holds its content:
/// `Some(old path)` after a failed rename, `None` after a failed create (any file at the path is
/// someone else's). Saves to such a path are dropped, so they can neither overwrite a file that
/// isn't the caller's nor create a second copy next to the real one; the caller saves again once
/// it has heard about the failure. A later create or rename that lands on the path clears it,
/// and a rename that moves a real file updates the entries pointing at it.
/// Shared by the worker and its handles, so a handle that outlives the worker honours it too.
type Stranded = Arc<Mutex<HashMap<PathBuf, Option<PathBuf>>>>;

fn lock(stranded: &Stranded) -> MutexGuard<'_, HashMap<PathBuf, Option<PathBuf>>> {
    stranded.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// After a write request arrives, wait this long for more before writing, so a burst of
/// changes (holding a key on the Settings tab) costs one fsync instead of one per keypress.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);
/// Longest `flush()` / shutdown waits for the worker (a hung network home dir must not hang quit).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// One queued file operation.
enum Op {
    Write(PathBuf, Vec<u8>),
    Create(PathBuf, Vec<u8>, Reporter),
    Delete(PathBuf),
    Rename(PathBuf, PathBuf, Option<Reporter>),
}

enum Msg {
    Op(Op),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// Cloneable handle for queueing writes on a [`PersistenceWorker`].
#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::Sender<Msg>,
    writer: Writer,
    stranded: Stranded,
}

impl PersistHandle {
    /// Queue `bytes` to replace `path`. Returns immediately; a later save of the same path that
    /// is queued before this one is written replaces it, unless a create, delete or rename was
    /// queued in between. If the worker has already shut down, the write happens on the calling
    /// thread so it is never lost.
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        self.queue(Op::Write(path, bytes));
    }

    /// Queue writing `bytes` to a new file at `path` with [`create_new_file`], after everything
    /// queued before it. Returns immediately. A file already at `path` is never replaced: the
    /// create fails, `reporter` hears about it, and later saves and deletes of `path` leave that
    /// file alone. `reporter` gets one [`PersistReport`] either way. If the worker has already
    /// shut down, the create happens on the calling thread.
    pub fn create(&self, path: PathBuf, bytes: Vec<u8>, reporter: Reporter) {
        self.queue(Op::Create(path, bytes, reporter));
    }

    /// Queue removing `path`, after everything queued before it. Returns immediately. A file
    /// that is already gone is not an error. If the worker has already shut down, the delete
    /// happens on the calling thread. After a failed rename onto `path`, the file still at the
    /// old name is the one removed; after a failed create of `path`, nothing is.
    pub fn delete(&self, path: PathBuf) {
        self.queue(Op::Delete(path));
    }

    /// Queue moving `from` to `to` with the operating system's rename, after everything queued
    /// before it. Returns immediately. A case-only rename (`road trip.m3u8` to `Road Trip.m3u8`)
    /// keeps its one file on a case-insensitive disk; any other file at `to` makes the rename
    /// fail instead of being replaced. If the worker has already shut down, the rename happens
    /// on the calling thread.
    ///
    /// When the rename fails (for any reason but `from` being gone), saves to `to` are dropped
    /// until something lands on `to`, so they never leave a second file next to `from`.
    ///
    /// Creates, deletes and renames always act on the file system directly; a custom [`Writer`]
    /// only replaces how saves are written.
    pub fn rename(&self, from: PathBuf, to: PathBuf) {
        self.queue(Op::Rename(from, to, None));
    }

    /// [`rename`](Self::rename), and tell `reporter` how it went (one [`PersistReport`]).
    pub fn rename_reporting(&self, from: PathBuf, to: PathBuf, reporter: Reporter) {
        self.queue(Op::Rename(from, to, Some(reporter)));
    }

    /// Send `op` to the worker, or apply it here when the worker has shut down.
    fn queue(&self, op: Op) {
        if let Err(mpsc::SendError(Msg::Op(op))) = self.tx.send(Msg::Op(op)) {
            apply_op(&op, &self.writer, &mut lock(&self.stranded));
        }
    }

    /// Block until every write queued before this call is on disk (or `FLUSH_TIMEOUT` passes).
    /// Returns false on timeout.
    pub fn flush(&self) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Msg::Flush(ack_tx)).is_err() {
            return true; // worker gone: everything it accepted was written before it exited
        }
        ack_rx.recv_timeout(FLUSH_TIMEOUT).is_ok()
    }
}

/// One background thread that writes Quickstream's own small files (settings, bookmarks) so
/// the UI thread never waits on disk. Dropping it writes everything still queued and joins it.
pub struct PersistenceWorker {
    handle: PersistHandle,
    done_rx: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl PersistenceWorker {
    /// Start a worker that writes with [`atomic_write`].
    pub fn spawn() -> Self {
        Self::spawn_with_writer(Arc::new(|p: &Path, b: &[u8]| atomic_write(p, b)))
    }

    /// Start a worker with a custom writer (tests use one that records or blocks).
    pub fn spawn_with_writer(writer: Writer) -> Self {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stranded: Stranded = Arc::default();
        let thread_writer = writer.clone();
        let thread_stranded = stranded.clone();
        let thread = std::thread::Builder::new()
            .name("qs-persist".into())
            .spawn(move || {
                worker_loop(rx, &thread_writer, &thread_stranded);
                let _ = done_tx.send(());
            })
            .ok();
        Self {
            handle: PersistHandle { tx, writer, stranded },
            done_rx,
            thread,
        }
    }

    pub fn handle(&self) -> PersistHandle {
        self.handle.clone()
    }

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
        self.handle.flush()
    }
}

impl Drop for PersistenceWorker {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Msg::Shutdown);
        if let Some(thread) = self.thread.take() {
            match self.done_rx.recv_timeout(FLUSH_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = thread.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log_warn("persistence worker still writing at shutdown; not waiting");
                }
            }
        }
    }
}

fn log_warn(message: &str) {
    crate::logger::log_raw("WARN", "persist", message);
}

fn write_logged(writer: &Writer, path: &Path, bytes: &[u8]) {
    if let Err(e) = writer(path, bytes) {
        log_warn(&format!("failed to write '{}': {}", path.display(), e));
    }
}

/// Remove `path`. A file that is already gone is what the caller asked for, so it is not logged.
fn delete_logged(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => log_warn(&format!("failed to delete '{}': {}", path.display(), e)),
    }
}

/// Apply one operation, keeping `stranded` up to date (see [`Stranded`]).
fn apply_op(op: &Op, writer: &Writer, stranded: &mut HashMap<PathBuf, Option<PathBuf>>) {
    match op {
        Op::Write(path, bytes) => {
            if stranded.contains_key(path) {
                log_warn(&format!(
                    "not writing '{}': creating or renaming onto it failed",
                    path.display()
                ));
            } else {
                write_logged(writer, path, bytes);
            }
        }
        Op::Create(path, bytes, reporter) => match create_new_file(path, bytes) {
            Ok(()) => {
                stranded.remove(path);
                reporter.done();
            }
            Err(e) => {
                log_warn(&format!("failed to create '{}': {}", path.display(), e));
                stranded.insert(path.clone(), None);
                reporter.send(PersistFailure::Create {
                    tag: reporter.tag,
                    path: path.clone(),
                    kind: e.kind(),
                    error: e.to_string(),
                });
            }
        },
        Op::Delete(path) => match stranded.get(path).cloned() {
            // The rename onto `path` failed, so the file to delete is still at its old name.
            Some(Some(real)) => {
                stranded.remove(path);
                delete_logged(&real);
            }
            Some(None) => log_warn(&format!(
                "not deleting '{}': creating it failed, so the file there is not ours",
                path.display()
            )),
            None => delete_logged(path),
        },
        Op::Rename(from, to, reporter) => {
            let done = || {
                if let Some(reporter) = reporter {
                    reporter.done();
                }
            };
            if let Some(real) = stranded.get(from).cloned() {
                // Renaming back onto the file that really holds the content (PR #65 review):
                // that file is ours again under `to`, so saves to `to` must reach it.
                if let Some(real) = real.clone().filter(|real| is_same_path(real, to)) {
                    stranded.remove(from);
                    match rename_no_replace(&real, to) {
                        Ok(()) => moved(stranded, &real, to),
                        Err(e) => log_warn(&format!(
                            "failed to rename '{}' to '{}': {}",
                            real.display(),
                            to.display(),
                            e
                        )),
                    }
                    done();
                    return;
                }
                // Nothing of ours is at `from`; `to` now stands for the same file `from` did.
                log_warn(&format!(
                    "not renaming '{}': creating or renaming onto it failed",
                    from.display()
                ));
                stranded.insert(to.clone(), real);
                done();
                return;
            }
            match rename_no_replace(from, to) {
                Ok(()) => {
                    moved(stranded, from, to);
                    done();
                }
                Err(e) => {
                    log_warn(&format!(
                        "failed to rename '{}' to '{}': {}",
                        from.display(),
                        to.display(),
                        e
                    ));
                    // With `from` gone nothing moved and nothing is in the way: a save to `to`
                    // just writes the file fresh. An earlier failure noted on `to` is stale now
                    // that the caller owns the name, and following it would reach another
                    // playlist's file (PR #65 review).
                    if e.kind() == io::ErrorKind::NotFound {
                        stranded.remove(to);
                        done();
                        return;
                    }
                    stranded.insert(to.clone(), Some(from.clone()));
                    if let Some(reporter) = reporter {
                        reporter.send(PersistFailure::Rename {
                            tag: reporter.tag,
                            from: from.clone(),
                            to: to.clone(),
                            kind: e.kind(),
                            error: e.to_string(),
                        });
                    }
                }
            }
        }
    }
}

/// `from` was just moved to `to`: `to` is ours, and an entry that pointed at the file under its
/// old name follows it, so the map keeps naming where each playlist's content really is.
fn moved(stranded: &mut HashMap<PathBuf, Option<PathBuf>>, from: &Path, to: &Path) {
    sync_parent_dir(to);
    stranded.remove(to);
    for real in stranded.values_mut().flatten() {
        if real == from {
            *real = to.to_path_buf();
        }
    }
}

/// Whether `a` and `b` name one file: the same path, or (on a disk that ignores case or Unicode
/// form) two spellings of one existing file.
fn is_same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => same_file(&ma, &mb, a, b),
        _ => false,
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
            Msg::Op(Op::Write(path, bytes)) => self.queue_write(path, bytes),
            Msg::Op(op) => self.ops.push(op),
            Msg::Flush(ack) => self.acks.push(ack),
            Msg::Shutdown => self.shutdown = true,
        }
    }

    /// A save replaces an earlier queued save of the same path (keeping that save's place), but
    /// never one queued before a create, delete or rename: moving the bytes ahead of it would
    /// let the create find them in its way, the delete remove them or the rename carry them to
    /// the new name. Any of those stops the merge, not only one naming this path, because on a
    /// case-insensitive disk `road trip.m3u8` and `Road Trip.m3u8` are one file.
    fn queue_write(&mut self, path: PathBuf, bytes: Vec<u8>) {
        for op in self.ops.iter_mut().rev() {
            match op {
                Op::Write(p, queued) if *p == path => {
                    *queued = bytes;
                    return;
                }
                Op::Write(..) => {}
                Op::Create(..) | Op::Delete(_) | Op::Rename(..) => break,
            }
        }
        self.ops.push(Op::Write(path, bytes));
    }

    /// A flush or shutdown is waiting: write now instead of waiting for more saves.
    fn urgent(&self) -> bool {
        self.shutdown || !self.acks.is_empty()
    }
}

fn worker_loop(rx: mpsc::Receiver<Msg>, writer: &Writer, stranded: &Stranded) {
    loop {
        let mut batch = Batch::default();
        match rx.recv() {
            Ok(msg) => batch.absorb(msg),
            Err(_) => return, // every handle dropped
        }
        while !batch.urgent() {
            match rx.recv_timeout(COALESCE_WINDOW) {
                Ok(msg) => batch.absorb(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    batch.shutdown = true;
                }
            }
        }
        while let Ok(msg) = rx.try_recv() {
            batch.absorb(msg);
        }
        {
            let mut stranded = lock(stranded);
            for op in &batch.ops {
                apply_op(op, writer, &mut stranded);
            }
        }
        for ack in batch.acks {
            let _ = ack.send(());
        }
        if batch.shutdown {
            return;
        }
    }
}
