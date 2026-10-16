//! Playback queue state and circular track traversal.

use crate::metadata::AudioMetadata;

#[derive(Debug, Clone, Default)]
pub struct PlaybackQueue {
    pub tracks: Vec<AudioMetadata>,
    pub current_index: usize,
}

impl PlaybackQueue {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            current_index: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn add_track(&mut self, track: AudioMetadata) {
        self.tracks.push(track);
    }

    pub fn add_tracks(&mut self, tracks: Vec<AudioMetadata>) {
        self.tracks.extend(tracks);
    }

    pub fn remove_track(&mut self, index: usize) -> Option<AudioMetadata> {
        if index < self.tracks.len() {
            let removed = self.tracks.remove(index);
            if self.current_index >= self.tracks.len() && !self.tracks.is_empty() {
                self.current_index = self.tracks.len() - 1;
            }
            Some(removed)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.current_index = 0;
    }

    pub fn current_track(&self) -> Option<&AudioMetadata> {
        self.tracks.get(self.current_index)
    }

    pub fn current_track_mut(&mut self) -> Option<&mut AudioMetadata> {
        self.tracks.get_mut(self.current_index)
    }

    pub fn next_track(&mut self, mode: &str) -> Option<&AudioMetadata> {
        if self.tracks.is_empty() {
            return None;
        }

        if mode == "repeat_one" {
            return self.current_track();
        }

        if self.current_index + 1 < self.tracks.len() {
            self.current_index += 1;
            self.current_track()
        } else if mode == "loop" {
            self.current_index = 0;
            self.current_track()
        } else {
            // "stop" mode
            None
        }
    }

    pub fn prev_track(&mut self) -> Option<&AudioMetadata> {
        if self.tracks.is_empty() {
            return None;
        }

        if self.current_index > 0 {
            self.current_index -= 1;
        } else {
            self.current_index = self.tracks.len() - 1;
        }
        self.current_track()
    }

    pub fn reorder_track(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);
        if self.current_index == from {
            self.current_index = to;
        }
        true
    }

    pub fn total_duration_seconds(&self) -> f64 {
        self.tracks.iter().map(|t| t.duration_seconds).sum()
    }

    /// Shuffle the queue in-place. If a track is actively playing, keep it at index 0 and shuffle the rest.
    pub fn shuffle(&mut self) {
        if self.tracks.len() <= 1 {
            return;
        }
        let mut rng = FastRng::new();
        if self.current_index < self.tracks.len() {
            let current = self.tracks.remove(self.current_index);
            rng.shuffle(&mut self.tracks);
            self.tracks.insert(0, current);
            self.current_index = 0;
        } else {
            rng.shuffle(&mut self.tracks);
            self.current_index = 0;
        }
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
}
