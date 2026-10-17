//! Crash-safe persistence for Quickstream's own config and cache files (never library files).
//!
//! - [`atomic_write`] replaces a file by writing `<file>.<pid>.tmp` in the same directory,
//!   fsyncing it and renaming it over the target, so a crash leaves either the old or the new
//!   content, never a truncated file.
//! - [`load_json_or_backup`] copies a file that fails to parse to `<file>.bak` before the caller
//!   falls back to defaults, so the next save can't silently destroy the user's settings.
//! - [`PersistenceWorker`] performs those writes on one background thread so the UI thread
//!   never waits on disk; it coalesces rapid saves and flushes on drop.

use serde::de::DeserializeOwned;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
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
    // Persist the rename itself. Best effort: not every filesystem supports fsync on a directory.
    if let Some(parent) = parent {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
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

/// Writes one file's bytes; the worker's default is [`atomic_write`].
pub type Writer = std::sync::Arc<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;

/// After a write request arrives, wait this long for more before writing, so a burst of
/// changes (holding a key on the Settings tab) costs one fsync instead of one per keypress.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);
/// Longest `flush()` / shutdown waits for the worker (a hung network home dir must not hang quit).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

enum Msg {
    Write(PathBuf, Vec<u8>),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// Cloneable handle for queueing writes on a [`PersistenceWorker`].
#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::Sender<Msg>,
    writer: Writer,
}

impl PersistHandle {
    /// Queue `bytes` to replace `path`. Returns immediately; a later save of the same path that
    /// is queued before this one is written replaces it. If the worker has already shut down,
    /// the write happens on the calling thread so it is never lost.
    pub fn save(&self, path: PathBuf, bytes: Vec<u8>) {
        if let Err(mpsc::SendError(Msg::Write(path, bytes))) = self.tx.send(Msg::Write(path, bytes)) {
            write_logged(&self.writer, &path, &bytes);
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
        let thread_writer = writer.clone();
        let thread = std::thread::Builder::new()
            .name("qs-persist".into())
            .spawn(move || {
                worker_loop(rx, &thread_writer);
                let _ = done_tx.send(());
            })
            .ok();
        Self {
            handle: PersistHandle { tx, writer },
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
                    crate::logger::log_raw(
                        "WARN",
                        "persist",
                        "persistence worker still writing at shutdown; not waiting",
                    );
                }
            }
        }
    }
}

fn write_logged(writer: &Writer, path: &Path, bytes: &[u8]) {
    if let Err(e) = writer(path, bytes) {
        crate::logger::log_raw(
            "WARN",
            "persist",
            &format!("failed to write '{}': {}", path.display(), e),
        );
    }
}

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

    /// A flush or shutdown is waiting: write now instead of waiting for more saves.
    fn urgent(&self) -> bool {
        self.shutdown || !self.acks.is_empty()
    }
}

fn worker_loop(rx: mpsc::Receiver<Msg>, writer: &Writer) {
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
        for (path, bytes) in &batch.writes {
            write_logged(writer, path, bytes);
        }
        for ack in batch.acks {
            let _ = ack.send(());
        }
        if batch.shutdown {
            return;
        }
    }
}
