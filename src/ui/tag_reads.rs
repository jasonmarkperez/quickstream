//! Tag reads shared by the UI's metadata workers (issue #52).
//!
//! Three kinds of worker read tags from the share: the browser's metadata fetch, the queue's
//! metadata pool and a playlist folder add. They share one set of the paths being read right
//! now, so no two of them read the same file at once. A worker that finds its path taken does
//! not read it: the browser and queue workers skip it, and a folder add waits for the other
//! read to end. Every read publishes its tags the same way (the on-disk index, the shared
//! [`MetadataCache`], then `metadata_tx`) before its path leaves the set, so a request that
//! skipped or waited still gets them: the UI refreshes browser labels and queue rows from
//! `metadata_tx`, and the folder add takes them from the cache. A read that fails publishes the
//! file's size if the reader found it ([`crate::metadata::UnreadableTags`]), so a file queued
//! with no size (issue #124) gets it from whichever worker read it, or that the file wasn't
//! found, so the queue can read it again later. A path leaves the set when its read ends, also
//! when the read fails or panics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::library::MetadataCache;
use crate::metadata::AudioMetadata;

/// Reads one file's tags. [`crate::metadata::extract_metadata`] except in tests, which use a
/// counting or blocking stand-in for the share.
pub type TagReader = Arc<dyn Fn(&Path) -> anyhow::Result<AudioMetadata> + Send + Sync>;

/// What a tag read publishes on `metadata_tx`.
#[derive(Debug, Clone, PartialEq)]
pub enum TagUpdate {
    /// The file's tags (also in the [`MetadataCache`] entries by then).
    Tags(AudioMetadata),
    /// The tags couldn't be read. `file_size` is the size the reader found (also in the cache,
    /// [`MetadataCache::unreadable_size`]), `None` when it couldn't find the file at all.
    Unreadable { path: PathBuf, file_size: Option<u64> },
}

impl From<AudioMetadata> for TagUpdate {
    fn from(meta: AudioMetadata) -> Self {
        TagUpdate::Tags(meta)
    }
}

/// How often a waiting folder add checks its stop flag.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// What a call to [`SharedTagReads`] did with a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagRead {
    /// This worker read the file and published its tags.
    Read,
    /// This worker read the file and the read failed; it published the size it found, if any.
    Failed,
    /// Another worker is reading the file and publishes its tags; this one skipped it.
    Busy,
    /// Another worker was reading the file; this one waited for that read to end. Its tags are
    /// in the cache now, unless that read failed.
    Waited,
    /// The stop flag was set while waiting for another worker's read.
    Stopped,
}

/// The paths being read, and a signal for workers waiting on one of them.
#[derive(Default)]
struct Reading {
    paths: Mutex<HashSet<PathBuf>>,
    ended: Condvar,
}

impl Reading {
    fn paths(&self) -> MutexGuard<'_, HashSet<PathBuf>> {
        self.paths.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Handle on the shared tag reads; clone it into each worker.
#[derive(Clone)]
pub struct SharedTagReads {
    reader: TagReader,
    reading: Arc<Reading>,
    cache: MetadataCache,
    metadata_tx: Sender<TagUpdate>,
}

/// One path marked as being read. Dropping it (also while a panic unwinds) clears the mark and
/// wakes the workers waiting for it.
struct Claim<'a> {
    reading: &'a Reading,
    path: PathBuf,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.reading.paths().remove(&self.path);
        self.reading.ended.notify_all();
    }
}

impl SharedTagReads {
    /// Reads publish their tags (or, failing, the file's size) to `cache` and `metadata_tx`.
    pub fn new(cache: MetadataCache, metadata_tx: Sender<TagUpdate>) -> Self {
        Self {
            reader: Arc::new(|path: &Path| crate::metadata::extract_metadata(path)),
            reading: Arc::default(),
            cache,
            metadata_tx,
        }
    }

    /// Replace the tag reader. Workers started before keep the reader they were given.
    pub fn set_reader(&mut self, reader: TagReader) {
        self.reader = reader;
    }

    /// Number of paths being read right now.
    pub fn in_flight(&self) -> usize {
        self.reading.paths().len()
    }

    /// Read `path` unless another worker is reading it right now ([`TagRead::Busy`]); that
    /// read publishes the tags, so the caller loses nothing by skipping.
    pub fn read_unless_busy(&self, path: &Path) -> TagRead {
        let claim = {
            let mut paths = self.reading.paths();
            if !paths.insert(path.to_path_buf()) {
                return TagRead::Busy;
            }
            self.claim(path)
        };
        self.read_claimed(claim)
    }

    /// Read `path`, or, when another worker is reading it, wait for that read to end instead
    /// of reading it again ([`TagRead::Waited`]: the tags are in the cache unless it failed).
    /// Gives up with [`TagRead::Stopped`] once `stop` is set.
    pub fn read_or_wait(&self, path: &Path, stop: &AtomicBool) -> TagRead {
        let claim = {
            let mut paths = self.reading.paths();
            if paths.contains(path) {
                while paths.contains(path) {
                    if stop.load(Ordering::Relaxed) {
                        return TagRead::Stopped;
                    }
                    paths = self
                        .reading
                        .ended
                        .wait_timeout(paths, WAIT_POLL)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                return TagRead::Waited;
            }
            paths.insert(path.to_path_buf());
            self.claim(path)
        };
        self.read_claimed(claim)
    }

    /// The claim on `path`, which the caller has just inserted into the set.
    fn claim(&self, path: &Path) -> Claim<'_> {
        Claim {
            reading: &self.reading,
            path: path.to_path_buf(),
        }
    }

    /// Read the claimed path and publish its tags, or the size a failed read found; the claim
    /// ends after publishing, so a worker that waited finds them in the cache. A failed read's
    /// size stays out of the on-disk index, so the next session reads the file again.
    fn read_claimed(&self, claim: Claim<'_>) -> TagRead {
        match (self.reader)(&claim.path) {
            Ok(meta) => {
                crate::library::LibraryManager::append_to_cache(&meta);
                self.cache.insert(meta.clone());
                let _ = self.metadata_tx.send(TagUpdate::Tags(meta));
                TagRead::Read
            }
            Err(err) => {
                let file_size = err
                    .downcast_ref::<crate::metadata::UnreadableTags>()
                    .map(|e| e.file_size);
                if let Some(size) = file_size {
                    self.cache.insert_unreadable(&claim.path, size);
                }
                let _ = self.metadata_tx.send(TagUpdate::Unreadable {
                    path: claim.path.clone(),
                    file_size,
                });
                TagRead::Failed
            }
        }
    }
}
