//! The playback queue's edits and its metadata pool: tracks added and removed, the queue cursor,
//! tags read for queued tracks (and applied to the browser rows that show them), and the queue's
//! paging keys.

use std::path::PathBuf;

use super::tag_reads::{SharedTagReads, TagUpdate};
use super::workers::WorkerRegistry;
use super::AppUI;

/// Start 4 threads that read tags for paths sent to the returned inbox through `tag_reads`, which
/// reports results on `metadata_tx` (a failed read the file's size, issue #124). A path another
/// worker is reading right now is skipped: that read reports it. The threads exit once the inbox
/// sender is dropped or `stop` is set (paths still queued in the inbox are then skipped).
fn spawn_queue_metadata_workers(
    workers: &mut WorkerRegistry,
    tag_reads: SharedTagReads,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::mpsc::Sender<PathBuf> {
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..4 {
        let rx = rx.clone();
        let tag_reads = tag_reads.clone();
        let stop = stop.clone();
        workers.spawn("queue-metadata", move || loop {
            let path = match rx.lock() {
                Ok(guard) => match guard.recv() {
                    Ok(p) => p,
                    Err(_) => break,
                },
                Err(_) => break,
            };
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tag_reads.read_unless_busy(&path);
        });
    }
    tx
}

impl AppUI {
    /// Resolve tags for queue tracks that still have placeholder metadata. Each path is read at
    /// most once per session by a persistent pool of 4 workers (a read that couldn't find the
    /// file leaves it to be read again by the next add, play, next or previous); results arrive
    /// via `metadata_rx`.
    /// A zero sample rate marks a placeholder that has an artist and duration (a playlist's
    /// `#EXTINF`) but no album or audio properties yet.
    pub fn start_queue_metadata_fetch(&mut self) {
        let candidates: Vec<PathBuf> = self
            .queue
            .tracks()
            .iter()
            .filter(|t| t.duration_seconds <= 0.0 || t.artist.is_empty() || t.sample_rate_hz == 0)
            .map(|t| t.filepath.clone())
            .collect();
        let new_paths: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|p| self.queue_metadata_attempted.insert(p.clone()))
            .collect();
        if new_paths.is_empty() {
            return;
        }

        let tag_reads = self.tag_reads.clone();
        let (workers, stop) = (&mut self.workers, self.queue_metadata_stop.clone());
        let tx = self
            .queue_metadata_tx
            .get_or_insert_with(|| spawn_queue_metadata_workers(workers, tag_reads, stop));
        for path in new_paths {
            let _ = tx.send(path);
        }
    }

    pub fn poll_pending_queue_adds(&mut self) {
        while let Ok(tracks) = self.queue_add_rx.try_recv() {
            let tracks: Vec<_> = tracks
                .into_iter()
                .map(|t| self.metadata_cache.get(&t.filepath).unwrap_or(t))
                .collect();
            let added = self.enqueue_tracks(tracks);
            self.status_message = format!("Added {} tracks (web)", added);
            self.update_web_state();
        }
    }

    /// Cached metadata for `path`, or a placeholder built from the path alone, with the size a
    /// failed tag read found for it earlier this session, if any. Never touches the disk, not
    /// even to stat the file (issue #124): the queue's tag read fills in the size.
    pub(super) fn quick_metadata(&self, path: &std::path::Path) -> crate::metadata::AudioMetadata {
        self.metadata_cache
            .get(path)
            .unwrap_or_else(|| crate::metadata::AudioMetadata {
                file_size: self.metadata_cache.unreadable_size(path).unwrap_or(0),
                ..crate::metadata::path_metadata(path)
            })
    }

    /// Put tracks in the queue with `put`, after registering their files with the web server's
    /// stream, then resolve missing tags on background workers. Returns what `put` returns
    /// (`None` when there are no tracks: `put` isn't called).
    fn queue_new_tracks<R>(
        &mut self,
        tracks: Vec<crate::metadata::AudioMetadata>,
        put: impl FnOnce(&mut Self, Vec<crate::metadata::AudioMetadata>) -> R,
    ) -> Option<R> {
        if tracks.is_empty() {
            return None;
        }
        for meta in &tracks {
            self.register_stream_file(&meta.filepath);
        }
        let result = put(self, tracks);
        self.start_queue_metadata_fetch();
        Some(result)
    }

    /// Append tracks to the queue and resolve missing tags on background workers.
    pub(super) fn enqueue_tracks(&mut self, tracks: Vec<crate::metadata::AudioMetadata>) -> usize {
        let added = tracks.len();
        self.queue_new_tracks(tracks, |app, tracks| app.queue.add_tracks(tracks));
        added
    }

    /// Insert tracks before queue position `index`, keeping the current track and the cursor on
    /// the rows they were on, and resolve missing tags on background workers.
    pub(super) fn insert_queue_tracks(&mut self, index: usize, tracks: Vec<crate::metadata::AudioMetadata>) {
        self.queue_new_tracks(tracks, |app, tracks| {
            let index = index.min(app.queue.len());
            if !app.queue.is_empty() && index <= app.queue_cursor {
                app.queue_cursor += tracks.len();
            }
            app.queue.insert_tracks(index, tracks);
        });
    }

    /// Insert tracks at random positions after the current track and before queue position
    /// `end`, keeping the current track, the order of the queued tracks and the cursor on the
    /// entry it was on, and resolve missing tags on background workers: the rest of a playlist
    /// arriving after a shuffle, mixed into the tracks that shuffle mixed (issue #122).
    pub(super) fn shuffle_queue_tracks_in(&mut self, tracks: Vec<crate::metadata::AudioMetadata>, end: usize) {
        self.queue_new_tracks(tracks, |app, tracks| {
            let moved = app.queue.shuffle_in(tracks, end);
            app.follow_queue_reorder(moved);
        });
    }

    /// Shuffle the queue (`z` / `Z`, the web dashboard's Shuffle, the continuous stream's
    /// reshuffle on a wrap): the current track goes in front and the cursor stays on the entry it
    /// was on (issue #123), as does the continuous stream. While the rest of a playlist `Enter`
    /// started is still being checked, that rest is shuffled in when it arrives (issue #122);
    /// while it waits for its first track to start, the shuffle is kept for that start, even with
    /// an empty queue (issue #135). `source` ends the status (`" (web)"`).
    pub(super) fn shuffle_queue(&mut self, source: &str) {
        let at_start = self.shuffle_playlist_at_start();
        if self.queue.is_empty() && at_start.is_none() {
            return;
        }
        if !self.queue.is_empty() {
            let moved = self.queue.shuffle();
            self.follow_queue_reorder(moved);
        }
        self.status_message = match at_start.or_else(|| self.shuffle_in_playlist_fill()) {
            Some(note) => format!("🔀 Queue shuffled{source}; {note}"),
            None => format!("🔀 Queue shuffled{source}"),
        };
        self.update_web_state();
    }

    /// The queue was reordered: `moved[i]` is the new position of the track that was at `i`.
    /// The cursor moves with its entry, and while the continuous stream plays, the stream is told
    /// where its tracks went when the new order is published (`update_web_state`), so it goes on
    /// in that order.
    fn follow_queue_reorder(&mut self, moved: Vec<usize>) {
        match moved.get(self.queue_cursor) {
            Some(&to) => self.queue_cursor = to,
            None => self.clamp_queue_cursor(),
        }
        if self.continuous_stream_active() {
            self.stream_reorder = Some(match self.stream_reorder.take() {
                // Reordered twice before a publish: where each track went in the end.
                Some(earlier) => earlier.iter().map(|&at| moved.get(at).copied().unwrap_or(at)).collect(),
                None => moved,
            });
        }
    }

    /// Keep the queue cursor on a real row after the queue shrinks.
    pub fn clamp_queue_cursor(&mut self) {
        self.queue_cursor = self.queue_cursor.min(self.queue.len().saturating_sub(1));
    }

    /// Remove a queue row (TUI `d` or the web dashboard). Removing the track that is playing or
    /// paused moves playback on to the track that took its place; emptying the queue stops it.
    pub fn remove_queue_track(&mut self, index: usize) -> Option<crate::metadata::AudioMetadata> {
        let removed = self.queue.remove(index)?;
        self.playlist_fill_row_removed(index);
        self.clamp_queue_cursor();
        if removed.was_current {
            if self.queue.is_empty() {
                let _ = self.player.stop();
            } else if self.player.is_playing() || self.player.is_paused() {
                self.play_current_track();
            }
        }
        Some(removed.track)
    }

    pub fn poll_background_metadata(&mut self) {
        let mut queue_updated = false;
        while let Ok(update) = self.metadata_rx.try_recv() {
            let meta = match update {
                TagUpdate::Tags(meta) => meta,
                // The tags couldn't be read: the queued rows get the size alone (issue #124),
                // keeping the rest of their placeholder. A file that wasn't found at all (a
                // share gone for a moment) is read again by the next add, play, next or previous.
                TagUpdate::Unreadable { path, file_size } => {
                    match file_size {
                        Some(size) => queue_updated |= self.queue.set_file_size(&path, size),
                        None => {
                            self.queue_metadata_attempted.remove(&path);
                        }
                    }
                    continue;
                }
            };
            let path = meta.filepath.clone();
            self.metadata_cache.insert(meta.clone());

            // 1. Update matching tracks in queue
            if self.queue.update_metadata(&meta) {
                queue_updated = true;
            }

            // 2. Update browser entries if active
            if self.settings_mgr.settings.metadata_view {
                if !self.in_bookmarks_view {
                    for (label, p, is_dir) in &mut self.browser_entries {
                        if !*is_dir && p == &path {
                            *label = meta.format_display_label(true);
                            break;
                        }
                    }
                }

                if let Some(parent) = path.parent() {
                    if let Some(cached) = self.browser_dir_cache.get_mut(parent) {
                        for (label, p, is_dir) in &mut cached.files {
                            if !*is_dir && p == &path {
                                *label = meta.format_display_label(true);
                                break;
                            }
                        }
                    }
                }
            }
        }

        if queue_updated {
            self.update_web_state();
        }
    }

    pub fn page_down_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let step = self.queue_page_height.max(1);
        if self.queue_cursor + step < len {
            self.queue_cursor += step;
        } else if self.queue_cursor < len - 1 {
            self.queue_cursor = len - 1;
        } else {
            self.queue_cursor = 0;
        }
    }

    pub fn page_up_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let step = self.queue_page_height.max(1);
        if self.queue_cursor >= step {
            self.queue_cursor -= step;
        } else if self.queue_cursor > 0 {
            self.queue_cursor = 0;
        } else {
            self.queue_cursor = len - 1;
        }
    }

    pub fn home_queue(&mut self) {
        if !self.queue.is_empty() {
            self.queue_cursor = 0;
        }
    }

    pub fn end_queue(&mut self) {
        if !self.queue.is_empty() {
            self.queue_cursor = self.queue.len().saturating_sub(1);
        }
    }
}
