//! Playback queue state and circular track traversal.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::metadata::AudioMetadata;

/// Source of every [`PlaybackQueue::revision`] value, shared by all queues in the process, so a
/// new queue, a clone or a replacement never repeats a revision another queue state had.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

/// The tracks to play and which one is current.
///
/// The fields are private so every change goes through a method that bumps [`Self::revision`];
/// caches built from the queue (the IN QUEUE path set, totals) rebuild when it moves.
#[derive(Debug)]
pub struct PlaybackQueue {
    tracks: Vec<AudioMetadata>,
    current_index: usize,
    revision: u64,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self {
            tracks: Vec::new(),
            current_index: 0,
            revision: next_revision(),
        }
    }
}

/// A clone gets a revision of its own: its tracks start out equal to the original's, but either
/// may change next, and a cache keyed on the revision must not mistake one for the other.
impl Clone for PlaybackQueue {
    fn clone(&self) -> Self {
        Self {
            tracks: self.tracks.clone(),
            current_index: self.current_index,
            revision: next_revision(),
        }
    }
}

/// A track taken out of the queue by [`PlaybackQueue::remove`].
#[derive(Debug, Clone)]
pub struct RemovedTrack {
    pub track: AudioMetadata,
    /// The removed track was the current one. `current_index` now points at the track that
    /// followed it (or the new last track); the caller decides whether to start playing it.
    pub was_current: bool,
}

/// What happens once the tracks [`PlaybackQueue::upcoming`] lists have played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEnd {
    /// Playback stops (stop mode).
    Stops,
    /// The current track plays again, and again (repeat-one mode); nothing else is next.
    RepeatOne,
    /// The queue loops: after the listed tracks (which already wrap past the end to the first
    /// track) the current one plays again. With a single track it simply repeats.
    Loops,
    /// The UPnP continuous stream reshuffles the queue and starts over, so what follows the
    /// listed tracks isn't known yet.
    Reshuffles,
}

/// The indices of the tracks that play after the current one, in play order, from
/// [`PlaybackQueue::upcoming`]; [`Upcoming::end`] says what happens after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upcoming {
    /// Next position to yield, counted from 0 and taken modulo `queue_len`.
    next: usize,
    queue_len: usize,
    left: usize,
    end: QueueEnd,
}

impl Upcoming {
    /// What happens once the listed tracks have played.
    pub fn end(&self) -> QueueEnd {
        self.end
    }
}

impl Iterator for Upcoming {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.left == 0 {
            return None;
        }
        let index = self.next % self.queue_len;
        self.next += 1;
        self.left -= 1;
        Some(index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Upcoming {}

impl PlaybackQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// A number that changes whenever the tracks, their order, their metadata or the current
    /// index change. Reads and calls that change nothing leave it as it is. Every value comes from
    /// one process-wide counter, so no two queues (new, cloned or replaced) ever share one.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision = next_revision();
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn tracks(&self) -> &[AudioMetadata] {
        &self.tracks
    }

    /// Index of the current track. It may be past the end while the queue is empty.
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    /// Make the track at `index` the current one (not clamped).
    pub fn set_current_index(&mut self, index: usize) {
        if self.current_index != index {
            self.current_index = index;
            self.bump();
        }
    }

    pub fn add_track(&mut self, track: AudioMetadata) {
        self.tracks.push(track);
        self.bump();
    }

    pub fn add_tracks(&mut self, tracks: Vec<AudioMetadata>) {
        if tracks.is_empty() {
            return;
        }
        self.tracks.extend(tracks);
        self.bump();
    }

    /// Insert `tracks` before position `index` (clamped to the end). `current_index` keeps naming
    /// the same track, so tracks inserted at or before it shift it along.
    pub fn insert_tracks(&mut self, index: usize, tracks: Vec<AudioMetadata>) {
        if tracks.is_empty() {
            return;
        }
        let index = index.min(self.tracks.len());
        let count = tracks.len();
        let had_tracks = !self.tracks.is_empty();
        self.tracks.splice(index..index, tracks);
        if had_tracks && index <= self.current_index {
            self.current_index += count;
        }
        self.bump();
    }

    /// Remove the track at `index`, keeping `current_index` on the same track. See [`Self::remove`].
    pub fn remove_track(&mut self, index: usize) -> Option<AudioMetadata> {
        self.remove(index).map(|r| r.track)
    }

    /// Remove the track at `index`. A removal before the current track shifts `current_index` down
    /// so it still names the same track. Removing the current track itself leaves `current_index`
    /// on the track that followed it (clamped to the new last track) and reports `was_current`.
    pub fn remove(&mut self, index: usize) -> Option<RemovedTrack> {
        if index >= self.tracks.len() {
            return None;
        }
        let was_current = index == self.current_index;
        let track = self.tracks.remove(index);
        if index < self.current_index {
            self.current_index -= 1;
        }
        if self.current_index >= self.tracks.len() {
            self.current_index = self.tracks.len().saturating_sub(1);
        }
        self.bump();
        Some(RemovedTrack { track, was_current })
    }

    /// Drop every track after position `index`, keeping tracks `0..=index`.
    pub fn truncate_after(&mut self, index: usize) {
        let keep = index.saturating_add(1);
        if keep < self.tracks.len() {
            self.tracks.truncate(keep);
            self.bump();
        }
    }

    pub fn clear(&mut self) {
        if self.tracks.is_empty() && self.current_index == 0 {
            return;
        }
        self.tracks.clear();
        self.current_index = 0;
        self.bump();
    }

    /// Replace every queued track whose path is `meta.filepath` with `meta`. Returns whether any
    /// track matched.
    pub fn update_metadata(&mut self, meta: &AudioMetadata) -> bool {
        let mut updated = false;
        for track in self.tracks.iter_mut().filter(|t| t.filepath == meta.filepath) {
            *track = meta.clone();
            updated = true;
        }
        if updated {
            self.bump();
        }
        updated
    }

    pub fn current_track(&self) -> Option<&AudioMetadata> {
        self.tracks.get(self.current_index)
    }

    /// The current track for editing. Bumps the revision whether or not it is then changed.
    pub fn current_track_mut(&mut self) -> Option<&mut AudioMetadata> {
        self.track_mut(self.current_index)
    }

    /// The track at `index` for editing. Bumps the revision whether or not it is then changed.
    pub fn track_mut(&mut self, index: usize) -> Option<&mut AudioMetadata> {
        if index >= self.tracks.len() {
            return None;
        }
        self.bump();
        self.tracks.get_mut(index)
    }

    pub fn next_track(&mut self, mode: &str) -> Option<&AudioMetadata> {
        if self.tracks.is_empty() {
            return None;
        }

        if mode == "repeat_one" {
            return self.current_track();
        }

        if self.current_index + 1 < self.tracks.len() {
            self.set_current_index(self.current_index + 1);
            self.current_track()
        } else if mode == "loop" {
            self.set_current_index(0);
            self.current_track()
        } else {
            // "stop" mode
            None
        }
    }

    /// The tracks that play after the current one, as [`Self::next_track`] in `mode` will play
    /// them (`wrap: false`), or as the UPnP continuous stream does (`wrap: true`): it plays on to
    /// the end of the queue whatever the mode, then reshuffles and starts over. Stop mode lists
    /// the tracks to the end of the queue; loop mode every other track, past the end to the
    /// first; repeat-one none, since the current track repeats.
    pub fn upcoming(&self, mode: &str, wrap: bool) -> Upcoming {
        let len = self.tracks.len();
        let next = self.current_index + 1;
        let to_end = len.saturating_sub(next);
        let (left, end) = if len <= 1 {
            let end = if mode == "repeat_one" && !wrap {
                QueueEnd::RepeatOne
            } else if mode == "loop" || wrap {
                QueueEnd::Loops
            } else {
                QueueEnd::Stops
            };
            (0, end)
        } else if wrap {
            (to_end, QueueEnd::Reshuffles)
        } else {
            match mode {
                "repeat_one" => (0, QueueEnd::RepeatOne),
                "loop" => (len - 1, QueueEnd::Loops),
                _ => (to_end, QueueEnd::Stops),
            }
        };
        Upcoming {
            next,
            queue_len: len,
            left,
            end,
        }
    }

    pub fn prev_track(&mut self) -> Option<&AudioMetadata> {
        if self.tracks.is_empty() {
            return None;
        }

        if self.current_index > 0 {
            self.set_current_index(self.current_index - 1);
        } else {
            self.set_current_index(self.tracks.len() - 1);
        }
        self.current_track()
    }

    /// Move the track at `from` to position `to`. `current_index` keeps naming the same track,
    /// whether that track is the one moved or another track moves across it.
    pub fn reorder_track(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        if from == to {
            return true;
        }
        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);
        let cur = self.current_index;
        if cur == from {
            self.current_index = to;
        } else if from < cur && to >= cur {
            self.current_index -= 1;
        } else if from > cur && to <= cur {
            self.current_index += 1;
        }
        self.bump();
        true
    }

    pub fn total_duration_seconds(&self) -> f64 {
        self.tracks.iter().map(|t| t.duration_seconds).sum()
    }

    /// Shuffle the queue in-place. If a track is actively playing, keep it at index 0 and shuffle the rest.
    /// Returns where each track went: entry `i` of the result is the new position of the track
    /// that was at `i`, so a caller can follow one entry even when a file is queued twice.
    pub fn shuffle(&mut self) -> Vec<usize> {
        let len = self.tracks.len();
        if len <= 1 {
            return (0..len).collect();
        }
        let mut rng = FastRng::new();
        // Each track with the position it had, so where it went is read off the result.
        let mut entries: Vec<(usize, AudioMetadata)> = self.tracks.drain(..).enumerate().collect();
        if self.current_index < len {
            let current = entries.remove(self.current_index);
            rng.shuffle(&mut entries);
            entries.insert(0, current);
        } else {
            rng.shuffle(&mut entries);
        }
        self.current_index = 0;
        let mut moved = vec![0; len];
        for (new, (old, track)) in entries.into_iter().enumerate() {
            moved[old] = new;
            self.tracks.push(track);
        }
        self.bump();
        moved
    }

    /// Insert `tracks` in random order at random positions after the current track (anywhere,
    /// when there is none) and before position `end`, leaving the current track and the order
    /// of the queued tracks as they are: the rest of a playlist arriving after a shuffle, mixed
    /// into the tracks that shuffle mixed. The tracks from `end` on (queued after the shuffle)
    /// stay after them, in their order. Returns where each track that was already queued went.
    pub fn shuffle_in(&mut self, mut tracks: Vec<AudioMetadata>, end: usize) -> Vec<usize> {
        let len = self.tracks.len();
        if tracks.is_empty() {
            return (0..len).collect();
        }
        let mut rng = FastRng::new();
        rng.shuffle(&mut tracks);
        let start = if self.current_index < len {
            self.current_index + 1
        } else {
            0
        };
        let end = end.clamp(start, len);
        let added = tracks.len();
        // One slot per track from `start` to `end`, old or new; shuffling the slots picks a
        // uniformly random interleaving that keeps each side's own order.
        let mut slots = vec![false; end - start];
        slots.resize(end - start + added, true);
        rng.shuffle(&mut slots);
        let later = self.tracks.split_off(end);
        let mut upcoming = self.tracks.split_off(start).into_iter();
        let mut added_tracks = tracks.into_iter();
        let mut moved: Vec<usize> = (0..start).collect();
        for is_new in slots {
            if is_new {
                self.tracks.extend(added_tracks.next());
            } else {
                moved.push(self.tracks.len());
                self.tracks.extend(upcoming.next());
            }
        }
        moved.extend((end..len).map(|old| old + added));
        self.tracks.extend(later);
        self.bump();
        moved
    }

    /// Shuffle only upcoming tracks in the queue from current_index + 1 forward.
    /// Preserves tracks 0..=current_index in place and does not alter current_index.
    pub fn shuffle_from_next(&mut self) {
        // Fewer than two upcoming tracks: nothing can move.
        if self.current_index.saturating_add(2) >= self.tracks.len() {
            return;
        }
        let mut rng = FastRng::new();
        rng.shuffle(&mut self.tracks[self.current_index + 1..]);
        self.bump();
    }
}

/// Lightweight, zero-external-dependency SplitMix64 pseudo-random number generator.
#[derive(Debug, Clone)]
pub struct FastRng {
    state: u64,
}

impl FastRng {
    /// Create a new RNG initialized from current system nanoseconds.
    pub fn new() -> Self {
        use std::time::SystemTime;
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x853c49e6748fea9b);
        Self {
            state: if seed == 0 { 0x853c49e6748fea9b } else { seed },
        }
    }

    /// Create an RNG with an explicit deterministic seed (useful for unit tests).
    pub fn with_seed(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x853c49e6748fea9b } else { seed },
        }
    }

    /// Generate next 64-bit pseudo-random integer.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Generate an integer in `[0, upper)`.
    pub fn gen_range(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        (self.next_u64() % (upper as u64)) as usize
    }

    /// Shuffle a slice in-place using the unbiased Fisher-Yates algorithm.
    pub fn shuffle<T>(&mut self, slice: &mut [T]) {
        if slice.len() <= 1 {
            return;
        }
        for i in (1..slice.len()).rev() {
            let j = self.gen_range(i + 1);
            slice.swap(i, j);
        }
    }
}

impl Default for FastRng {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_test_track(title: &str, duration: f64) -> AudioMetadata {
        AudioMetadata {
            filepath: PathBuf::from(format!("/music/{}.mp3", title)),
            filename: format!("{}.mp3", title),
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            genre: "Genre".to_string(),
            format_name: "MP3".to_string(),
            duration_seconds: duration,
            file_size: 1000,
            bitrate_kbps: 320,
            sample_rate_hz: 44100,
            channels: 2,
        }
    }

    #[test]
    fn test_queue_navigation_loop() {
        let mut q = PlaybackQueue::new();
        q.add_track(make_test_track("Song 1", 100.0));
        q.add_track(make_test_track("Song 2", 200.0));

        assert_eq!(q.current_track().unwrap().title, "Song 1");
        assert_eq!(q.next_track("loop").unwrap().title, "Song 2");
        assert_eq!(q.next_track("loop").unwrap().title, "Song 1"); // Wraps around
        assert_eq!(q.prev_track().unwrap().title, "Song 2"); // Wraps backwards
    }

    #[test]
    fn test_queue_stop_at_end() {
        let mut q = PlaybackQueue::new();
        q.add_track(make_test_track("Song 1", 100.0));
        assert!(q.next_track("stop").is_none());
    }

    #[test]
    fn test_queue_removal_and_reorder() {
        let mut q = PlaybackQueue::new();
        q.add_track(make_test_track("Song 1", 100.0));
        q.add_track(make_test_track("Song 2", 200.0));
        q.add_track(make_test_track("Song 3", 300.0));

        assert_eq!(q.len(), 3);
        assert_eq!(q.total_duration_seconds(), 600.0);

        q.reorder_track(0, 2);
        assert_eq!(q.tracks[0].title, "Song 2");
        assert_eq!(q.tracks[2].title, "Song 1");

        let removed = q.remove_track(1).unwrap();
        assert_eq!(removed.title, "Song 3");
        assert_eq!(q.len(), 2);
    }

    fn queue_of(n: usize, current: usize) -> PlaybackQueue {
        let mut q = PlaybackQueue::new();
        for i in 1..=n {
            q.add_track(make_test_track(&format!("Song {}", i), 100.0));
        }
        q.current_index = current;
        q
    }

    fn current_title(q: &PlaybackQueue) -> String {
        q.current_track().map(|t| t.title.clone()).unwrap_or_default()
    }

    #[test]
    fn test_remove_before_current_keeps_current_track() {
        let mut q = queue_of(5, 3); // Song 4
        let removed = q.remove(1).unwrap();
        assert_eq!(removed.track.title, "Song 2");
        assert!(!removed.was_current);
        assert_eq!(q.current_index, 2);
        assert_eq!(current_title(&q), "Song 4");
    }

    #[test]
    fn test_remove_after_current_keeps_index() {
        let mut q = queue_of(5, 1); // Song 2
        let removed = q.remove(3).unwrap();
        assert!(!removed.was_current);
        assert_eq!(q.current_index, 1);
        assert_eq!(current_title(&q), "Song 2");
    }

    #[test]
    fn test_remove_current_moves_to_following_track() {
        let mut q = queue_of(5, 2); // Song 3
        let removed = q.remove(2).unwrap();
        assert_eq!(removed.track.title, "Song 3");
        assert!(removed.was_current);
        assert_eq!(q.current_index, 2);
        assert_eq!(current_title(&q), "Song 4");
    }

    #[test]
    fn test_remove_current_last_track_clamps_to_new_last() {
        let mut q = queue_of(3, 2); // Song 3
        let removed = q.remove(2).unwrap();
        assert!(removed.was_current);
        assert_eq!(q.current_index, 1);
        assert_eq!(current_title(&q), "Song 2");
    }

    #[test]
    fn test_remove_only_track_empties_queue() {
        let mut q = queue_of(1, 0);
        let removed = q.remove(0).unwrap();
        assert!(removed.was_current);
        assert!(q.is_empty());
        assert_eq!(q.current_index, 0);
        assert!(q.current_track().is_none());
    }

    #[test]
    fn test_remove_out_of_range_is_none() {
        let mut q = queue_of(2, 1);
        assert!(q.remove(2).is_none());
        assert!(q.remove_track(5).is_none());
        assert_eq!(q.len(), 2);
        assert_eq!(q.current_index, 1);
    }

    #[test]
    fn test_remove_track_wrapper_adjusts_current_index() {
        let mut q = queue_of(4, 2); // Song 3
        assert_eq!(q.remove_track(0).unwrap().title, "Song 1");
        assert_eq!(current_title(&q), "Song 3");
    }

    #[test]
    fn test_reorder_moving_earlier_track_past_current_keeps_current() {
        let mut q = queue_of(5, 2); // Song 3
        assert!(q.reorder_track(0, 4));
        assert_eq!(q.current_index, 1);
        assert_eq!(current_title(&q), "Song 3");
    }

    #[test]
    fn test_reorder_moving_later_track_before_current_keeps_current() {
        let mut q = queue_of(5, 2); // Song 3
        assert!(q.reorder_track(4, 0));
        assert_eq!(q.current_index, 3);
        assert_eq!(current_title(&q), "Song 3");
    }

    #[test]
    fn test_reorder_onto_current_slot_keeps_current() {
        // Moving a later track into the current slot pushes the current track down one.
        let mut q = queue_of(5, 2); // Song 3
        assert!(q.reorder_track(4, 2));
        assert_eq!(current_title(&q), "Song 3");
        // Moving an earlier track into the current slot pulls the current track up one.
        let mut q = queue_of(5, 2);
        assert!(q.reorder_track(0, 2));
        assert_eq!(current_title(&q), "Song 3");
    }

    #[test]
    fn test_reorder_current_track_follows_it() {
        let mut q = queue_of(5, 1); // Song 2
        assert!(q.reorder_track(1, 4));
        assert_eq!(q.current_index, 4);
        assert_eq!(current_title(&q), "Song 2");
    }

    #[test]
    fn test_reorder_tracks_on_one_side_of_current_keeps_index() {
        let mut q = queue_of(6, 1); // Song 2
        assert!(q.reorder_track(3, 5));
        assert_eq!(q.current_index, 1);
        assert_eq!(current_title(&q), "Song 2");
        assert!(!q.reorder_track(0, 6));
        assert_eq!(current_title(&q), "Song 2");
    }

    #[test]
    fn test_fast_rng_deterministic() {
        let mut rng1 = FastRng::with_seed(123456);
        let mut rng2 = FastRng::with_seed(123456);
        for _ in 0..10 {
            assert_eq!(rng1.next_u64(), rng2.next_u64());
        }

        let mut rng = FastRng::with_seed(42);
        for _ in 0..50 {
            let r = rng.gen_range(7);
            assert!(r < 7);
        }
    }

    #[test]
    fn test_fast_rng_shuffle() {
        let mut rng = FastRng::with_seed(999);
        let mut items = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let original = items.clone();
        rng.shuffle(&mut items);
        // All items still present
        let mut sorted = items.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        // Likely not identical to original order
        assert_ne!(items, original);
    }

    #[test]
    fn test_queue_shuffle_preserves_current() {
        let mut q = PlaybackQueue::new();
        for i in 1..=10 {
            q.add_track(make_test_track(&format!("Song {}", i), 100.0));
        }
        q.current_index = 3; // "Song 4"
        let current_title = q.current_track().unwrap().title.clone();
        assert_eq!(current_title, "Song 4");

        q.shuffle();
        assert_eq!(q.len(), 10);
        assert_eq!(q.current_index, 0);
        assert_eq!(q.current_track().unwrap().title, "Song 4");

        // Verify all 10 songs still exist
        let mut titles: Vec<String> = q.tracks.iter().map(|t| t.title.clone()).collect();
        titles.sort();
        let mut expected: Vec<String> = (1..=10).map(|i| format!("Song {}", i)).collect();
        expected.sort();
        assert_eq!(titles, expected);
    }

    #[test]
    fn test_queue_shuffle_from_next() {
        let mut q = PlaybackQueue::new();
        for i in 1..=10 {
            q.add_track(make_test_track(&format!("Song {}", i), 100.0));
        }
        q.current_index = 2; // "Song 3"

        q.shuffle_from_next();
        assert_eq!(q.len(), 10);
        assert_eq!(q.current_index, 2);
        // Tracks 0, 1, 2 must be strictly untouched
        assert_eq!(q.tracks[0].title, "Song 1");
        assert_eq!(q.tracks[1].title, "Song 2");
        assert_eq!(q.tracks[2].title, "Song 3");

        // Tracks 3..10 must contain all songs 4..10
        let mut upcoming: Vec<String> = q.tracks[3..].iter().map(|t| t.title.clone()).collect();
        upcoming.sort();
        let mut expected: Vec<String> = (4..=10).map(|i| format!("Song {}", i)).collect();
        expected.sort();
        assert_eq!(upcoming, expected);
    }

    #[test]
    fn test_queue_shuffle_empty_and_single() {
        let mut q = PlaybackQueue::new();
        q.shuffle();
        assert_eq!(q.len(), 0);

        q.add_track(make_test_track("Solo", 120.0));
        q.shuffle();
        assert_eq!(q.len(), 1);
        assert_eq!(q.current_index, 0);
        assert_eq!(q.tracks[0].title, "Solo");
    }

    #[test]
    fn test_queue_shuffle_reports_where_each_track_went() {
        // Issue #123: the queue cursor follows its entry through a shuffle.
        let mut q = PlaybackQueue::new();
        for i in 1..=10 {
            q.add_track(make_test_track(&format!("Song {}", i), 100.0));
        }
        q.current_index = 4;
        let before: Vec<String> = q.tracks.iter().map(|t| t.title.clone()).collect();
        let moved = q.shuffle();
        assert_eq!(moved.len(), 10);
        assert_eq!(moved[4], 0, "the current track goes in front");
        for (old, title) in before.iter().enumerate() {
            assert_eq!(&q.tracks[moved[old]].title, title, "track {old}");
        }
        assert_eq!(PlaybackQueue::new().shuffle(), Vec::<usize>::new());
    }

    #[test]
    fn test_queue_shuffle_in_keeps_the_current_track_and_the_queued_order() {
        // Issue #122: the rest of a playlist arriving after a shuffle is shuffled in.
        for _ in 0..20 {
            let mut q = PlaybackQueue::new();
            for i in 1..=6 {
                q.add_track(make_test_track(&format!("Song {}", i), 100.0));
            }
            q.current_index = 2;
            let before: Vec<String> = q.tracks.iter().map(|t| t.title.clone()).collect();
            let added: Vec<AudioMetadata> = (1..=5).map(|i| make_test_track(&format!("New {}", i), 100.0)).collect();
            let revision = q.revision();
            let moved = q.shuffle_in(added, usize::MAX);
            assert_ne!(q.revision(), revision);
            assert_eq!(q.len(), 11);
            assert_eq!(q.current_index, 2);
            assert_eq!(q.current_track().unwrap().title, "Song 3");
            for (old, title) in before.iter().enumerate() {
                assert_eq!(&q.tracks[moved[old]].title, title, "track {old}");
            }
            assert_eq!(&moved[..3], &[0, 1, 2], "the current track and those before it stay");
            assert!(moved.windows(2).all(|w| w[0] < w[1]), "queued tracks keep their order");
            let mut new: Vec<String> = q.tracks[3..]
                .iter()
                .map(|t| t.title.clone())
                .filter(|t| t.starts_with("New"))
                .collect();
            new.sort();
            assert_eq!(new, ["New 1", "New 2", "New 3", "New 4", "New 5"]);
        }
        let mut empty = PlaybackQueue::new();
        assert!(empty.shuffle_in(vec![make_test_track("Only", 1.0)], 0).is_empty());
        assert_eq!(empty.len(), 1);
        assert_eq!(empty.current_track().unwrap().title, "Only");
    }

    #[test]
    fn test_queue_shuffle_in_leaves_the_tracks_from_its_end_in_order_after_the_rest() {
        // The rest of a playlist arriving after a shuffle is mixed into the tracks that shuffle
        // mixed, rows 1 to 4 here; tracks queued after the shuffle (rows 5 and 6) stay after
        // them, in their order (PR #127 review).
        for _ in 0..20 {
            let mut q = PlaybackQueue::new();
            for i in 0..7 {
                q.add_track(make_test_track(&format!("Song {}", i), 100.0));
            }
            let added: Vec<AudioMetadata> = (1..=4).map(|i| make_test_track(&format!("New {}", i), 100.0)).collect();
            let moved = q.shuffle_in(added, 5);
            let titles: Vec<String> = q.tracks.iter().map(|t| t.title.clone()).collect();
            assert_eq!(titles.len(), 11);
            assert_eq!(titles[0], "Song 0", "the current track stays first");
            assert_eq!(titles[9..], ["Song 5", "Song 6"], "{titles:?}");
            let old: Vec<&String> = titles[1..9].iter().filter(|t| t.starts_with("Song")).collect();
            assert_eq!(old, ["Song 1", "Song 2", "Song 3", "Song 4"], "{titles:?}");
            for (from, &to) in moved.iter().enumerate() {
                assert_eq!(titles[to], format!("Song {from}"), "moved {moved:?}");
            }
        }
    }
}
