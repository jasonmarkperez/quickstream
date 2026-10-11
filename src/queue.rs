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
}
