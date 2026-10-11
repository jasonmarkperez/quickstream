"""Unit tests for metadata and audio format handling."""

import os
from pathlib import Path
import tempfile
import unittest

from quickstream.metadata import (
    SUPPORTED_EXTENSIONS,
    AudioMetadata,
    extract_metadata,
    get_mime_type,
    is_supported_audio_file,
)


class TestMetadata(unittest.TestCase):
    """Test metadata and format identification."""

    def test_supported_extensions(self):
        """Verify extension and MIME type recognition."""
        self.assertTrue(is_supported_audio_file("song.mp3"))
        self.assertTrue(is_supported_audio_file("track.flac"))
        self.assertTrue(is_supported_audio_file("recording.wav"))
        self.assertTrue(is_supported_audio_file("recording.wave"))
        self.assertTrue(is_supported_audio_file("master.aiff"))
        self.assertTrue(is_supported_audio_file("master.aif"))
        self.assertFalse(is_supported_audio_file("video.mp4"))
        self.assertFalse(is_supported_audio_file("document.txt"))

        self.assertEqual(get_mime_type("song.mp3"), "audio/mpeg")
        self.assertEqual(get_mime_type("track.flac"), "audio/flac")
        self.assertEqual(get_mime_type("recording.wav"), "audio/wav")
        self.assertEqual(get_mime_type("master.aiff"), "audio/aiff")

    def test_duration_formatting(self):
        """Test formatting duration into MM:SS and HH:MM:SS."""
        m1 = AudioMetadata(
            filepath="test.mp3",
            filename="test.mp3",
            file_size=1024,
            mime_type="audio/mpeg",
            format_name="MP3",
            duration_seconds=125.0,
        )
        self.assertEqual(m1.duration_formatted, "02:05")

        m2 = AudioMetadata(
            filepath="test.flac",
            filename="test.flac",
            file_size=1024,
            mime_type="audio/flac",
            format_name="FLAC",
            duration_seconds=3665.0,
        )
        self.assertEqual(m2.duration_formatted, "01:01:05")

    def test_synthesized_wav_parsing(self):
        """Test pure-Python WAV parser with a synthesized WAV file."""
        import struct

        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as f:
            temp_path = f.name
            sample_rate = 44100
            channels = 2
            bits_per_sample = 16
            byte_rate = sample_rate * channels * (bits_per_sample // 8)
            block_align = channels * (bits_per_sample // 8)
            num_samples = 44100  # 1 second of audio
            data_size = num_samples * block_align

            # RIFF header
            f.write(b"RIFF")
            f.write(struct.pack("<I", 36 + data_size))
            f.write(b"WAVE")
            # fmt chunk
            f.write(b"fmt ")
            f.write(struct.pack("<I", 16))
            f.write(struct.pack("<H", 1))  # PCM
            f.write(struct.pack("<H", channels))
            f.write(struct.pack("<I", sample_rate))
            f.write(struct.pack("<I", byte_rate))
            f.write(struct.pack("<H", block_align))
            f.write(struct.pack("<H", bits_per_sample))
            # data chunk
            f.write(b"data")
            f.write(struct.pack("<I", data_size))
            f.write(b"\x00" * data_size)

        try:
            meta = extract_metadata(temp_path)
            self.assertEqual(meta.format_name, "WAV")
            self.assertEqual(meta.channels, 2)
            self.assertEqual(meta.sample_rate, 44100)
            self.assertAlmostEqual(meta.duration_seconds, 1.0, places=1)
        finally:
            if os.path.exists(temp_path):
                os.remove(temp_path)


if __name__ == "__main__":
    unittest.main()
