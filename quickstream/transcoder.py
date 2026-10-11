"""Transcoding helper for audio formats using ffmpeg."""

import logging
from pathlib import Path
import shutil
import subprocess
import tempfile
from typing import Optional

from quickstream.metadata import AudioMetadata, extract_metadata

logger = logging.getLogger("quickstream.transcoder")


class AudioTranscoder:
    """Handles audio transcoding to standard WAV using ffmpeg when needed."""

    def __init__(self):
        self.ffmpeg_path = shutil.which("ffmpeg")
        self._temp_files: list[Path] = []

    def is_available(self) -> bool:
        """Check if ffmpeg is installed and available."""
        return self.ffmpeg_path is not None

    def transcode_to_wav(
        self,
        metadata: AudioMetadata,
        sample_rate: int = 44100,
    ) -> Optional[AudioMetadata]:
        """Transcode input file to standard 16-bit PCM WAV for maximum DLNA compatibility."""
        if not self.is_available():
            logger.warning("ffmpeg is not installed. Cannot transcode.")
            return None

        # Create temporary file
        temp_dir = tempfile.gettempdir()
        target_name = f"quickstream_transcoded_{Path(metadata.filename).stem}.wav"
        output_path = Path(temp_dir) / target_name

        cmd = [
            self.ffmpeg_path,
            "-y",
            "-i",
            metadata.filepath,
            "-c:a",
            "pcm_s16le",
            "-ar",
            str(sample_rate),
            str(output_path),
        ]

        try:
            res = subprocess.run(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if res.returncode != 0:
                logger.error("ffmpeg transcoding failed: %s", res.stderr)
                return None

            self._temp_files.append(output_path)
            new_metadata = extract_metadata(output_path)
            # Retain original title, artist, album if present
            if metadata.title:
                new_metadata.title = metadata.title
            if metadata.artist:
                new_metadata.artist = metadata.artist
            if metadata.album:
                new_metadata.album = metadata.album

            return new_metadata
        except Exception as e:
            logger.error("Error during transcoding: %s", e)
            return None

    def cleanup(self) -> None:
        """Remove any generated temporary transcoded files."""
        for temp_file in self._temp_files:
            try:
                if temp_file.exists():
                    temp_file.unlink()
            except Exception:
                pass
        self._temp_files.clear()
