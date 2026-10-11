"""Unit tests for PlaybackQueue and queue management."""

import os
from pathlib import Path
import tempfile
import unittest

from quickstream.metadata import AudioMetadata
from quickstream.queue import PlaybackQueue


class TestPlaybackQueue(unittest.TestCase):
    """Test queue queue and navigation."""

    def setUp(self):
        self.queue = PlaybackQueue()
        self.temp_dir = tempfile.TemporaryDirectory()

        # Create mock audio files
        self.f1 = Path(self.temp_dir.name) / "01_intro.mp3"
        self.f2 = Path(self.temp_dir.name) / "02_verse.flac"
        self.f3 = Path(self.temp_dir.name) / "03_chorus.wav"
        self.f4 = Path(self.temp_dir.name) / "not_audio.txt"

        for f in [self.f1, self.f2, self.f3, self.f4]:
            f.write_bytes(b"dummy data 1234567890")

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_add_directory(self):
        """Verify directory scan adds only supported audio files in alphabetical order."""
        added = self.queue.add_directory(self.temp_dir.name)
        self.assertEqual(len(added), 3)
        self.assertEqual(len(self.queue), 3)
        self.assertEqual(self.queue[0].filename, "01_intro.mp3")
        self.assertEqual(self.queue[1].filename, "02_verse.flac")
        self.assertEqual(self.queue[2].filename, "03_chorus.wav")

    def test_navigation(self):
        """Test next and previous track movement."""
        self.queue.add_directory(self.temp_dir.name)
        self.assertEqual(self.queue.current_index, 0)
        self.assertEqual(self.queue.get_current_track().filename, "01_intro.mp3")

        # Next
        next_track = self.queue.next_track()
        self.assertIsNotNone(next_track)
        self.assertEqual(self.queue.current_index, 1)
        self.assertEqual(next_track.filename, "02_verse.flac")

        # Previous
        prev_track = self.queue.previous_track()
        self.assertIsNotNone(prev_track)
        self.assertEqual(self.queue.current_index, 0)
        self.assertEqual(prev_track.filename, "01_intro.mp3")

    def test_remove_and_clear(self):
        """Test removing track and clearing."""
        self.queue.add_directory(self.temp_dir.name)
        removed = self.queue.remove(1)
        self.assertEqual(removed.filename, "02_verse.flac")
        self.assertEqual(len(self.queue), 2)

        self.queue.clear()
        self.assertEqual(len(self.queue), 0)
        self.assertIsNone(self.queue.get_current_track())


if __name__ == "__main__":
    unittest.main()
