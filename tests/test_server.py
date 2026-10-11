"""Unit tests for the HTTP media streaming server request handling."""

import io
import os
from pathlib import Path
import tempfile
import unittest

from quickstream.metadata import AudioMetadata
from quickstream.server import DLNAStreamRequestHandler


class MockSocket:
    """In-memory mock socket for testing HTTP request handlers without network sockets."""

    def __init__(self, request_bytes: bytes):
        self._rfile = io.BytesIO(request_bytes)
        self._wfile = io.BytesIO()

    def makefile(self, mode: str, *args, **kwargs):
        if "r" in mode:
            return self._rfile
        elif "w" in mode:
            return self._wfile
        raise ValueError(f"Unsupported mode: {mode}")

    def sendall(self, data: bytes):
        self._wfile.write(data)

    def close(self):
        pass


class MockServer:
    """Mock server object providing metadata to DLNAStreamRequestHandler."""

    def __init__(self, metadata: AudioMetadata):
        self.metadata = metadata

    def get_file_metadata(self, filename: str) -> Optional[AudioMetadata]:
        return self.metadata


def run_handler(request_text: str, metadata: AudioMetadata) -> tuple[int, dict[str, str], bytes]:
    """Execute DLNAStreamRequestHandler and return (status_code, headers_dict, body_bytes)."""
    sock = MockSocket(request_text.encode("utf-8"))
    server = MockServer(metadata)

    # Initialize handler
    DLNAStreamRequestHandler(sock, ("127.0.0.1", 54321), server)

    output = sock._wfile.getvalue()
    # Split headers and body
    header_part, _, body = output.partition(b"\r\n\r\n")

    lines = header_part.decode("utf-8", errors="replace").split("\r\n")
    status_line = lines[0] if lines else ""
    status_code = 0
    if len(status_line.split()) >= 2:
        status_code = int(status_line.split()[1])

    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()

    return status_code, headers, body


class TestServerHandler(unittest.TestCase):
    """Test HTTP range streaming and DLNA headers."""

    def setUp(self):
        # Create a test audio dummy file of 1000 bytes
        self.temp_file = tempfile.NamedTemporaryFile(suffix=".mp3", delete=False)
        self.dummy_data = bytes([i % 256 for i in range(1000)])
        self.temp_file.write(self.dummy_data)
        self.temp_file.close()

        self.metadata = AudioMetadata(
            filepath=self.temp_file.name,
            filename=Path(self.temp_file.name).name,
            file_size=len(self.dummy_data),
            mime_type="audio/mpeg",
            format_name="MP3",
            duration_seconds=10.0,
        )

    def tearDown(self):
        if os.path.exists(self.temp_file.name):
            os.remove(self.temp_file.name)

    def test_full_get_request(self):
        """Test standard HTTP GET request returns 200 OK and complete file."""
        req = f"GET /stream/{self.metadata.filename} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        status, headers, body = run_handler(req, self.metadata)

        self.assertEqual(status, 200)
        self.assertEqual(headers.get("content-type"), "audio/mpeg")
        self.assertEqual(headers.get("content-length"), "1000")
        self.assertEqual(headers.get("accept-ranges"), "bytes")
        self.assertIn("Streaming", headers.get("transfermode.dlna.org", ""))
        self.assertEqual(len(body), 1000)
        self.assertEqual(body, self.dummy_data)

    def test_range_request(self):
        """Test HTTP Byte-Range partial content request returns 206 and exact slice."""
        req = (
            f"GET /stream/{self.metadata.filename} HTTP/1.1\r\n"
            f"Host: localhost\r\n"
            f"Range: bytes=100-199\r\n"
            f"\r\n"
        )
        status, headers, body = run_handler(req, self.metadata)

        self.assertEqual(status, 206)
        self.assertEqual(headers.get("content-range"), "bytes 100-199/1000")
        self.assertEqual(headers.get("content-length"), "100")
        self.assertEqual(len(body), 100)
        self.assertEqual(body, self.dummy_data[100:200])

    def test_open_ended_range_request(self):
        """Test Range: bytes=800- reads to the end of the file."""
        req = (
            f"GET /stream/{self.metadata.filename} HTTP/1.1\r\n"
            f"Host: localhost\r\n"
            f"Range: bytes=800-\r\n"
            f"\r\n"
        )
        status, headers, body = run_handler(req, self.metadata)

        self.assertEqual(status, 206)
        self.assertEqual(headers.get("content-range"), "bytes 800-999/1000")
        self.assertEqual(headers.get("content-length"), "200")
        self.assertEqual(len(body), 200)
        self.assertEqual(body, self.dummy_data[800:])

    def test_head_request(self):
        """Test HTTP HEAD request returns headers but no body."""
        req = f"HEAD /stream/{self.metadata.filename} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        status, headers, body = run_handler(req, self.metadata)

        self.assertEqual(status, 200)
        self.assertEqual(headers.get("content-length"), "1000")
        self.assertEqual(len(body), 0)

    def test_offset_get_request_mp3(self):
        """Test HTTP GET with ?pos= returns 200 OK starting from frame sync."""
        import struct
        # Build an MP3 file with ID3 tag and valid sync markers
        id3_header = b"ID3\x03\x00\x00\x00\x00\x00\x0a" + (b"X" * 10)  # 20 bytes total ID3
        # MPEG-1 Layer 3 sync header: 0xFF, 0xFB, 0x90, 0x64
        frame_sync = b"\xff\xfb\x90\x64"
        frame_data = frame_sync + (b"\x00" * 414)
        mp3_content = id3_header + (frame_data * 10)

        temp_mp3 = tempfile.NamedTemporaryFile(suffix=".mp3", delete=False)
        temp_mp3.write(mp3_content)
        temp_mp3.close()

        mp3_meta = AudioMetadata(
            filepath=temp_mp3.name,
            filename=Path(temp_mp3.name).name,
            file_size=len(mp3_content),
            mime_type="audio/mpeg",
            format_name="MP3",
            duration_seconds=10.0,
        )

        try:
            req = f"GET /stream/{mp3_meta.filename}?pos=5.00 HTTP/1.1\r\nHost: localhost\r\n\r\n"
            status, headers, body = run_handler(req, mp3_meta)

            self.assertEqual(status, 200)
            self.assertEqual(headers.get("content-type"), "audio/mpeg")
            # First bytes of response must be a valid MP3 frame sync
            self.assertEqual(body[:4], frame_sync)
            self.assertLess(len(body), len(mp3_content))
            self.assertEqual(headers.get("content-length"), str(len(body)))
        finally:
            if os.path.exists(temp_mp3.name):
                os.remove(temp_mp3.name)

    def test_offset_get_request_wav(self):
        """Test HTTP GET with ?pos= on WAV synthesizes valid WAV header and streams PCM."""
        import struct
        pcm_rate = 44100
        channels = 2
        bps = 16
        align = channels * (bps // 8)
        byte_rate = pcm_rate * align
        pcm_seconds = 2.0
        pcm_size = int(pcm_seconds * byte_rate)
        pcm_data = bytes([i % 256 for i in range(pcm_size)])

        # Construct valid 44-byte WAV file
        wav_hdr = bytearray()
        wav_hdr.extend(b"RIFF")
        wav_hdr.extend(struct.pack("<I", 36 + pcm_size))
        wav_hdr.extend(b"WAVEfmt ")
        wav_hdr.extend(struct.pack("<I", 16))
        wav_hdr.extend(struct.pack("<HHIIHH", 1, channels, pcm_rate, byte_rate, align, bps))
        wav_hdr.extend(b"data")
        wav_hdr.extend(struct.pack("<I", pcm_size))
        wav_content = bytes(wav_hdr) + pcm_data

        temp_wav = tempfile.NamedTemporaryFile(suffix=".wav", delete=False)
        temp_wav.write(wav_content)
        temp_wav.close()

        wav_meta = AudioMetadata(
            filepath=temp_wav.name,
            filename=Path(temp_wav.name).name,
            file_size=len(wav_content),
            mime_type="audio/wav",
            format_name="WAV",
            duration_seconds=pcm_seconds,
        )

        try:
            req = f"GET /stream/{wav_meta.filename}?pos=1.00 HTTP/1.1\r\nHost: localhost\r\n\r\n"
            status, headers, body = run_handler(req, wav_meta)

            self.assertEqual(status, 200)
            self.assertEqual(headers.get("content-type"), "audio/wav")
            # First bytes must be RIFF header
            self.assertTrue(body.startswith(b"RIFF"))
            # Expected remaining PCM size is 1 second worth = 176400 bytes + 44 bytes header = 176444
            expected_remaining_pcm = int(1.0 * byte_rate)
            expected_total = 44 + expected_remaining_pcm
            self.assertEqual(len(body), expected_total)
            self.assertEqual(headers.get("content-length"), str(expected_total))
            # Verify body payload after 44-byte header matches PCM slice from 1.0s
            self.assertEqual(body[44:], pcm_data[expected_remaining_pcm:])
        finally:
            if os.path.exists(temp_wav.name):
                os.remove(temp_wav.name)


if __name__ == "__main__":
    unittest.main()
