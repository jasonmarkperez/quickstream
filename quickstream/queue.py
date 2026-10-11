"""PlaybackQueue and media queue management for Quickstream."""

from dataclasses import dataclass, field
import os
from pathlib import Path
from typing import Iterator, List, Optional

from quickstream.metadata import AudioMetadata, extract_metadata, is_supported_audio_file


@dataclass
class PlaybackQueue:
    """Manages audio track queue and playback order."""

    tracks: List[AudioMetadata] = field(default_factory=list)
    current_index: int = -1

    def add_file(self, filepath: str | Path) -> Optional[AudioMetadata]:
        """Inspect and append a supported audio file to queue."""
        path = Path(filepath).resolve()
        if not path.is_file() or not is_supported_audio_file(path):
            return None
        try:
            meta = extract_metadata(path)
            self.tracks.append(meta)
            if self.current_index == -1:
                self.current_index = 0
            return meta
        except Exception:
            return None

    def add_directory(self, dir_path: str | Path, recursive: bool = False) -> List[AudioMetadata]:
        """Scan directory and add all supported audio files, sorted alphabetically."""
        path = Path(dir_path).resolve()
        if not path.is_dir():
            return []

        added: List[AudioMetadata] = []
        iterator: Iterator[Path] = path.rglob("*") if recursive else path.iterdir()

        # Sort paths for consistent order
        sorted_files = sorted(
            [p for p in iterator if p.is_file() and is_supported_audio_file(p)],
            key=lambda p: str(p).lower(),
        )

        for f in sorted_files:
            meta = self.add_file(f)
            if meta:
                added.append(meta)

        return added

    def remove(self, index: int) -> Optional[AudioMetadata]:
        """Remove a track by index."""
        if 0 <= index < len(self.tracks):
            removed = self.tracks.pop(index)
            if len(self.tracks) == 0:
                self.current_index = -1
            elif self.current_index >= len(self.tracks):
                self.current_index = len(self.tracks) - 1
            return removed
        return None

    def clear(self) -> None:
        """Clear all tracks from queue."""
        self.tracks.clear()
        self.current_index = -1

    def get_current_track(self) -> Optional[AudioMetadata]:
        """Get currently active track."""
        if 0 <= self.current_index < len(self.tracks):
            return self.tracks[self.current_index]
        return None

    def set_current_index(self, index: int) -> Optional[AudioMetadata]:
        """Set current playback track by index."""
        if 0 <= index < len(self.tracks):
            self.current_index = index
            return self.tracks[index]
        return None

    def next_track(self) -> Optional[AudioMetadata]:
        """Advance to next track if available."""
        if self.has_next():
            self.current_index += 1
            return self.tracks[self.current_index]
        return None

    def previous_track(self) -> Optional[AudioMetadata]:
        """Move to previous track if available."""
        if self.current_index > 0:
            self.current_index -= 1
            return self.tracks[self.current_index]
        return None

    def has_next(self) -> bool:
        """Check if there is a subsequent track in queue."""
        return self.current_index + 1 < len(self.tracks)

    def __len__(self) -> int:
        return len(self.tracks)

    def __getitem__(self, index: int) -> AudioMetadata:
        return self.tracks[index]
