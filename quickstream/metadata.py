"""Audio metadata extraction and format handling for Quickstream."""

from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
from typing import Optional

SUPPORTED_EXTENSIONS = {
    ".mp3": "audio/mpeg",
    ".flac": "audio/flac",
    ".wav": "audio/wav",
    ".wave": "audio/wav",
    ".aiff": "audio/aiff",
    ".aif": "audio/aiff",
}

# Secondary MIME types recognized by some DLNA renderers
DLNA_MIME_FALLBACKS = {
    "audio/wav": "audio/x-wav",
    "audio/aiff": "audio/x-aiff",
}


@dataclass
class AudioMetadata:
    """Audio metadata container."""

    filepath: str
    filename: str
    file_size: int
    mime_type: str
    format_name: str
    duration_seconds: float = 0.0
    title: str = ""
    artist: str = ""
    album: str = ""
    bitrate: int = 0
    sample_rate: int = 0
    channels: int = 2

    @property
    def duration_formatted(self) -> str:
        """Format duration as MM:SS or HH:MM:SS."""
        total_sec = int(self.duration_seconds)
        hours = total_sec // 3600
        minutes = (total_sec % 3600) // 60
        seconds = total_sec % 60
        if hours > 0:
            return f"{hours:02d}:{minutes:02d}:{seconds:02d}"
        return f"{minutes:02d}:{seconds:02d}"

    @property
    def display_title(self) -> str:
        """User-friendly title string."""
        if self.title:
            return self.title
        # Strip extension
        return Path(self.filename).stem


def is_supported_audio_file(filepath: str | Path) -> bool:
    """Check if the file has a supported audio extension."""
    ext = Path(filepath).suffix.lower()
    return ext in SUPPORTED_EXTENSIONS


def get_mime_type(filepath: str | Path) -> str:
    """Return the primary MIME type for the audio file."""
    ext = Path(filepath).suffix.lower()
    return SUPPORTED_EXTENSIONS.get(ext, "application/octet-stream")


def _read_80bit_extended_float(bytes10: bytes) -> float:
    """Read IEEE 754 extended 80-bit float used in AIFF COMM chunk."""
    if len(bytes10) < 10:
        return 0.0
    exponent = struct.unpack(">H", bytes10[0:2])[0]
    mantissa = struct.unpack(">Q", bytes10[2:10])[0]
    if exponent == 0 and mantissa == 0:
        return 0.0
    sign = -1 if (exponent & 0x8000) else 1
    exponent = (exponent & 0x7FFF) - 16383
    # normalized float value
    val = sign * mantissa * (2.0 ** (exponent - 63))
    return float(val)


def _parse_aiff_pure_python(filepath: str) -> Optional[dict]:
    """Parse basic metadata from AIFF file without external dependencies."""
    try:
        with open(filepath, "rb") as f:
            header = f.read(12)
            if len(header) < 12 or header[:4] != b"FORM":
                return None
            form_type = header[8:12]
            if form_type not in (b"AIFF", b"AIFC"):
                return None

            while True:
                chunk_header = f.read(8)
                if len(chunk_header) < 8:
                    break
                chunk_id = chunk_header[:4]
                chunk_size = struct.unpack(">I", chunk_header[4:8])[0]

                if chunk_id == b"COMM":
                    comm_data = f.read(chunk_size)
                    if len(comm_data) >= 18:
                        channels = struct.unpack(">h", comm_data[0:2])[0]
                        num_frames = struct.unpack(">I", comm_data[2:6])[0]
                        sample_size = struct.unpack(">h", comm_data[6:8])[0]
                        sample_rate = int(_read_80bit_extended_float(comm_data[8:18]))
                        duration = (
                            float(num_frames) / float(sample_rate)
                            if sample_rate > 0
                            else 0.0
                        )
                        bitrate = channels * sample_rate * sample_size
                        return {
                            "channels": channels,
                            "sample_rate": sample_rate,
                            "duration": duration,
                            "bitrate": bitrate,
                        }
                    break
                else:
                    # Skip chunk (AIFF chunks are padded to even byte boundary)
                    skip_bytes = chunk_size + (chunk_size % 2)
                    f.seek(skip_bytes, os.SEEK_CUR)
    except Exception:
        pass
    return None


def _parse_wav_pure_python(filepath: str) -> Optional[dict]:
    """Parse basic metadata from WAV file without external dependencies."""
    try:
        with open(filepath, "rb") as f:
            header = f.read(12)
            if len(header) < 12 or header[:4] != b"RIFF" or header[8:12] != b"WAVE":
                return None

            channels = 2
            sample_rate = 44100
            byte_rate = 0
            bits_per_sample = 16
            data_size = 0

            while True:
                chunk_header = f.read(8)
                if len(chunk_header) < 8:
                    break
                chunk_id = chunk_header[:4]
                chunk_size = struct.unpack("<I", chunk_header[4:8])[0]

                if chunk_id == b"fmt ":
                    fmt_data = f.read(chunk_size)
                    if len(fmt_data) >= 16:
                        channels = struct.unpack("<H", fmt_data[2:4])[0]
                        sample_rate = struct.unpack("<I", fmt_data[4:8])[0]
                        byte_rate = struct.unpack("<I", fmt_data[8:12])[0]
                        bits_per_sample = struct.unpack("<H", fmt_data[14:16])[0]
                elif chunk_id == b"data":
                    data_size = chunk_size
                    f.seek(chunk_size, os.SEEK_CUR)
                else:
                    f.seek(chunk_size + (chunk_size % 2), os.SEEK_CUR)

            if byte_rate > 0 and data_size > 0:
                duration = float(data_size) / float(byte_rate)
                return {
                    "channels": channels,
                    "sample_rate": sample_rate,
                    "duration": duration,
                    "bitrate": byte_rate * 8,
                }
    except Exception:
        pass
    return None


def _extract_via_ffprobe(filepath: str) -> Optional[dict]:
    """Extract metadata using ffprobe if installed."""
    if not shutil.which("ffprobe"):
        return None

    try:
        cmd = [
            "ffprobe",
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            str(filepath),
        ]
        result = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        if result.returncode != 0:
            return None

        data = json.loads(result.stdout)
        fmt = data.get("format", {})
        tags = fmt.get("tags", {})
        # Normalize tags to lower case keys
        lower_tags = {k.lower(): v for k, v in tags.items()}

        duration = float(fmt.get("duration", 0.0))
        bitrate = int(fmt.get("bit_rate", 0))

        sample_rate = 0
        channels = 2
        for stream in data.get("streams", []):
            if stream.get("codec_type") == "audio":
                sample_rate = int(stream.get("sample_rate", 0))
                channels = int(stream.get("channels", 2))
                if duration == 0.0 and "duration" in stream:
                    duration = float(stream.get("duration", 0.0))
                break

        return {
            "title": lower_tags.get("title", ""),
            "artist": lower_tags.get("artist", lower_tags.get("album_artist", "")),
            "album": lower_tags.get("album", ""),
            "duration": duration,
            "bitrate": bitrate,
            "sample_rate": sample_rate,
            "channels": channels,
        }
    except Exception:
        return None


def extract_metadata(filepath: str | Path) -> AudioMetadata:
    """Extract full metadata for an audio file."""
    path = Path(filepath).resolve()
    if not path.exists():
        raise FileNotFoundError(f"Audio file not found: {path}")

    ext = path.suffix.lower()
    if ext not in SUPPORTED_EXTENSIONS:
        raise ValueError(
            f"Unsupported format '{ext}'. Supported: {', '.join(SUPPORTED_EXTENSIONS.keys())}"
        )

    file_size = path.stat().st_size
    mime_type = SUPPORTED_EXTENSIONS[ext]
    format_name = ext.lstrip(".").upper()
    if format_name == "WAVE":
        format_name = "WAV"
    elif format_name == "AIF":
        format_name = "AIFF"

    # Default metadata
    metadata = AudioMetadata(
        filepath=str(path),
        filename=path.name,
        file_size=file_size,
        mime_type=mime_type,
        format_name=format_name,
        title=path.stem,
    )

    # Attempt ffprobe first (comprehensive & extracts ID3/FLAC tags)
    ff_data = _extract_via_ffprobe(str(path))
    if ff_data:
        if ff_data.get("title"):
            metadata.title = ff_data["title"]
        metadata.artist = ff_data.get("artist", "")
        metadata.album = ff_data.get("album", "")
        metadata.duration_seconds = ff_data.get("duration", 0.0)
        metadata.bitrate = ff_data.get("bitrate", 0)
        metadata.sample_rate = ff_data.get("sample_rate", 0)
        metadata.channels = ff_data.get("channels", 2)
        return metadata

    # Pure-Python fallbacks for WAV and AIFF
    if format_name == "WAV":
        wav_data = _parse_wav_pure_python(str(path))
        if wav_data:
            metadata.duration_seconds = wav_data.get("duration", 0.0)
            metadata.bitrate = wav_data.get("bitrate", 0)
            metadata.sample_rate = wav_data.get("sample_rate", 0)
            metadata.channels = wav_data.get("channels", 2)
    elif format_name == "AIFF":
        aiff_data = _parse_aiff_pure_python(str(path))
        if aiff_data:
            metadata.duration_seconds = aiff_data.get("duration", 0.0)
            metadata.bitrate = aiff_data.get("bitrate", 0)
            metadata.sample_rate = aiff_data.get("sample_rate", 0)
            metadata.channels = aiff_data.get("channels", 2)

    return metadata
