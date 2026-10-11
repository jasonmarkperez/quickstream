"""HTTP Media Streaming Server with DLNA and HTTP Byte-Range support."""

from http import HTTPStatus
import http.server
import logging
import os
from pathlib import Path
import re
import socket
import socketserver
import struct
import threading
from typing import Callable, Dict, Optional
import urllib.parse

from quickstream.metadata import AudioMetadata

logger = logging.getLogger("quickstream.server")


def get_id3v2_size(filepath: Path) -> int:
    """Read ID3v2 tag size if present at the start of the file."""
    try:
        with open(filepath, "rb") as f:
            header = f.read(10)
            if len(header) == 10 and header[:3] == b"ID3":
                size = (
                    ((header[6] & 0x7F) << 21)
                    | ((header[7] & 0x7F) << 14)
                    | ((header[8] & 0x7F) << 7)
                    | (header[9] & 0x7F)
                )
                footer_size = 10 if (header[5] & 0x10) else 0
                return 10 + size + footer_size
    except Exception:
        pass
    return 0


def find_mp3_frame_sync(filepath: Path, start_offset: int, max_scan: int = 16384) -> int:
    """Scan forward from start_offset to find the next valid MP3 frame sync marker."""
    try:
        with open(filepath, "rb") as f:
            f.seek(start_offset)
            buf = f.read(max_scan)
            for i in range(len(buf) - 3):
                if buf[i] == 0xFF and (buf[i + 1] & 0xE0) == 0xE0:
                    # Layer check (00 reserved)
                    if (buf[i + 1] & 0x06) == 0:
                        continue
                    # MPEG version check (01 reserved)
                    if (buf[i + 1] & 0x18) == 0x08:
                        continue
                    # Bitrate index check (1111 invalid)
                    if (buf[i + 2] & 0xF0) == 0xF0:
                        continue
                    # Sampling frequency check (11 reserved)
                    if (buf[i + 2] & 0x0C) == 0x0C:
                        continue
                    return start_offset + i
    except Exception:
        pass
    return start_offset


def find_mp3_stream_offset(filepath: Path, metadata: AudioMetadata, offset_sec: float) -> int:
    """Calculate byte offset for MP3 stream aligned to nearest frame sync."""
    try:
        id3_size = get_id3v2_size(filepath)
        total_size = metadata.file_size
        audio_size = max(0, total_size - id3_size)
        dur = metadata.duration_seconds if metadata.duration_seconds > 0 else 1.0
        ratio = max(0.0, min(0.995, offset_sec / dur))
        est_offset = id3_size + int(audio_size * ratio)
        return find_mp3_frame_sync(filepath, est_offset)
    except Exception:
        return 0


def find_wav_pcm_offset(filepath: Path, offset_sec: float) -> tuple[int, Optional[bytes]]:
    """Calculate byte offset for WAV PCM and synthesize a 44-byte WAV header."""
    try:
        with open(filepath, "rb") as f:
            header = f.read(12)
            if len(header) < 12 or header[:4] != b"RIFF" or header[8:12] != b"WAVE":
                return 0, None

            fmt_info = None
            data_offset = 0
            data_size = 0

            while True:
                chunk_header = f.read(8)
                if len(chunk_header) < 8:
                    break
                cid = chunk_header[:4]
                csize = struct.unpack("<I", chunk_header[4:8])[0]

                if cid == b"fmt ":
                    fmt_data = f.read(csize)
                    if len(fmt_data) >= 16:
                        audio_fmt, channels, srate, brate, align, bps = struct.unpack(
                            "<HHIIHH", fmt_data[:16]
                        )
                        fmt_info = (audio_fmt, channels, srate, brate, align, bps)
                elif cid == b"data":
                    data_offset = f.tell()
                    data_size = csize
                    break
                else:
                    f.seek(csize + (csize % 2), os.SEEK_CUR)

            if fmt_info and data_offset > 0 and data_size > 0:
                audio_fmt, channels, srate, brate, align, bps = fmt_info
                pcm_offset = int(offset_sec * brate)
                if align > 0:
                    pcm_offset = (pcm_offset // align) * align
                pcm_offset = min(pcm_offset, data_size)
                file_seek_pos = data_offset + pcm_offset
                remaining_pcm = max(0, data_size - pcm_offset)

                hdr = bytearray()
                hdr.extend(b"RIFF")
                hdr.extend(struct.pack("<I", 36 + remaining_pcm))
                hdr.extend(b"WAVEfmt ")
                hdr.extend(struct.pack("<I", 16))
                hdr.extend(struct.pack("<HHIIHH", audio_fmt, channels, srate, brate, align, bps))
                hdr.extend(b"data")
                hdr.extend(struct.pack("<I", remaining_pcm))
                return file_seek_pos, bytes(hdr)
    except Exception:
        pass
    return 0, None


def calculate_stream_offset(
    filepath: Path,
    metadata: AudioMetadata,
    offset_sec: float,
) -> tuple[int, Optional[bytes]]:
    """Determine byte seek offset in file and any header bytes to prepend."""
    if offset_sec <= 0.0:
        return 0, None

    ext = filepath.suffix.lower()
    if ext in (".wav", ".wave") or metadata.mime_type == "audio/wav":
        return find_wav_pcm_offset(filepath, offset_sec)
    elif ext == ".mp3" or metadata.mime_type == "audio/mpeg":
        return find_mp3_stream_offset(filepath, metadata, offset_sec), None
    else:
        dur = metadata.duration_seconds if metadata.duration_seconds > 0 else 1.0
        ratio = max(0.0, min(0.995, offset_sec / dur))
        seek_pos = int(metadata.file_size * ratio)
        return seek_pos, None


class DLNAStreamRequestHandler(http.server.BaseHTTPRequestHandler):
    """HTTP request handler supporting HTTP Byte-Range and DLNA streaming headers."""

    protocol_version = "HTTP/1.1"
    server: "DLNAStreamServer"

    def log_message(self, format: str, *args) -> None:
        """Log HTTP requests to logger instead of stderr."""
        logger.info("%s - - [%s] %s", self.client_address[0], self.log_date_time_string(), format % args)

    def do_HEAD(self) -> None:
        """Handle HTTP HEAD requests (often used by renderers to probe size & ranges)."""
        self._serve(head_only=True)

    def do_GET(self) -> None:
        """Handle HTTP GET requests."""
        self._serve(head_only=False)

    def _serve(self, head_only: bool = False) -> None:
        # Determine which file is requested
        parsed_url = urllib.parse.urlparse(self.path)
        parsed_path = parsed_url.path
        raw_name = Path(parsed_path).name
        req_filename = urllib.parse.unquote(raw_name)

        metadata: Optional[AudioMetadata] = self.server.get_file_metadata(req_filename)
        if not metadata:
            # Fallback to current metadata on server
            metadata = self.server.metadata

        if not metadata:
            logger.warning("File not found on server: %s", req_filename)
            self.send_error(HTTPStatus.NOT_FOUND, "File not found")
            return

        filepath = Path(metadata.filepath)
        if not filepath.exists():
            logger.warning("File not found on disk: %s", filepath)
            self.send_error(HTTPStatus.NOT_FOUND, "File not found on disk")
            return

        client_ip = self.client_address[0] if hasattr(self, "client_address") and self.client_address else "unknown"
        if hasattr(self.server, "last_client_ip"):
            self.server.last_client_ip = client_ip

        total_size = metadata.file_size
        mime_type = metadata.mime_type

        # Parse offset query parameter if present (?pos=12.5 or ?offset=12.5)
        offset_sec = 0.0
        query_params = urllib.parse.parse_qs(parsed_url.query)
        pos_param = query_params.get("pos") or query_params.get("offset")
        if pos_param:
            try:
                offset_sec = max(0.0, float(pos_param[0]))
            except (ValueError, TypeError):
                offset_sec = 0.0

        base_file_seek, prepend_data = calculate_stream_offset(filepath, metadata, offset_sec)
        virtual_size = (len(prepend_data) if prepend_data else 0) + (total_size - base_file_seek)

        # Check for Range header
        range_header = self.headers.get("Range")
        req_start = 0
        req_end = virtual_size - 1
        is_range_request = False

        if range_header:
            match = re.match(r"^bytes=(\d*)-(\d*)$", range_header.strip())
            if match:
                start_str, end_str = match.groups()
                if start_str and end_str:
                    req_start = int(start_str)
                    req_end = int(end_str)
                elif start_str:
                    req_start = int(start_str)
                    req_end = virtual_size - 1
                elif end_str:
                    req_start = virtual_size - int(end_str)
                    req_end = virtual_size - 1

                if req_start >= virtual_size or req_end >= virtual_size or req_start > req_end:
                    logger.warning("Invalid Range requested: %s for virtual size %d", range_header, virtual_size)
                    self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
                    self.send_header("Content-Range", f"bytes */{virtual_size}")
                    self.end_headers()
                    return

                is_range_request = True

        content_length = req_end - req_start + 1

        logger.info(
            "Serving %s (method: %s, client: %s, offset: %.2fs, range: %s, bytes: %d-%d/%d)",
            metadata.filename,
            self.command,
            client_ip,
            offset_sec,
            range_header if is_range_request else "full",
            req_start,
            req_end,
            virtual_size,
        )

        if is_range_request:
            self.send_response(HTTPStatus.PARTIAL_CONTENT)
            self.send_header("Content-Range", f"bytes {req_start}-{req_end}/{virtual_size}")
        else:
            self.send_response(HTTPStatus.OK)

        self.send_header("Content-Type", mime_type)
        self.send_header("Content-Length", str(content_length))
        self.send_header("Accept-Ranges", "bytes")
        # DLNA Headers
        self.send_header("transferMode.dlna.org", "Streaming")
        self.send_header(
            "contentFeatures.dlna.org",
            "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
        )

        req_conn = self.headers.get("Connection", "").lower()
        if "close" in req_conn:
            self.send_header("Connection", "close")
            self.close_connection = True
        else:
            self.send_header("Connection", "keep-alive")

        self.end_headers()

        if head_only:
            return

        # Stream chunked bytes to client
        chunk_size = 64 * 1024  # 64 KB chunks
        remaining = content_length
        streamed_count = 0

        try:
            # First, send synthesized header slice if applicable
            file_start = base_file_seek
            if prepend_data:
                hdr_len = len(prepend_data)
                if req_start < hdr_len:
                    hdr_slice = prepend_data[req_start : min(hdr_len, req_start + remaining)]
                    self.wfile.write(hdr_slice)
                    self.wfile.flush()
                    remaining -= len(hdr_slice)
                    streamed_count += len(hdr_slice)
                    if hasattr(self.server, "total_bytes_sent"):
                        self.server.total_bytes_sent += len(hdr_slice)
                    file_start = base_file_seek
                else:
                    file_start = base_file_seek + (req_start - hdr_len)
            else:
                file_start = base_file_seek + req_start

            if remaining > 0:
                with open(filepath, "rb") as f:
                    f.seek(file_start)
                    while remaining > 0:
                        read_bytes = min(remaining, chunk_size)
                        chunk = f.read(read_bytes)
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        self.wfile.flush()
                        remaining -= len(chunk)
                        streamed_count += len(chunk)
                        if hasattr(self.server, "total_bytes_sent"):
                            self.server.total_bytes_sent += len(chunk)
            logger.info("Streamed %d bytes to %s", streamed_count, client_ip)
        except (BrokenPipeError, ConnectionResetError, OSError):
            logger.info("Client %s closed connection after %d bytes", client_ip, streamed_count)
            self.close_connection = True
        except Exception as e:
            logger.warning("Stream transfer exception from %s: %s", client_ip, e)
            self.close_connection = True

    def finish(self) -> None:
        """Finish request handling safely without raising BrokenPipeError."""
        try:
            if hasattr(self, "wfile") and not self.wfile.closed:
                self.wfile.flush()
        except Exception:
            pass
        try:
            super().finish()
        except Exception:
            pass


class DLNAStreamServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """Threaded HTTP server customized for audio streaming."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        initial_metadata: Optional[AudioMetadata] = None,
        bind_and_activate: bool = True,
    ):
        super().__init__(server_address, DLNAStreamRequestHandler, bind_and_activate)
        try:
            self.socket.setsockopt(socket.SOL_SOCKET, 0x1022, 1)  # SO_NOSIGPIPE on Darwin / macOS
        except Exception:
            pass
        self.metadata = initial_metadata
        self.files_by_name: Dict[str, AudioMetadata] = {}
        self.total_bytes_sent: int = 0
        self.last_client_ip: str = ""
        self._active_sockets: set[socket.socket] = set()
        self._socket_lock = threading.Lock()
        if initial_metadata:
            self.add_metadata(initial_metadata)

    def get_request(self):
        """Track accepted client connection socket for clean termination."""
        sock, addr = super().get_request()
        try:
            sock.setsockopt(socket.SOL_SOCKET, 0x1022, 1)  # SO_NOSIGPIPE on Darwin / macOS
        except Exception:
            pass
        with self._socket_lock:
            self._active_sockets.add(sock)
        return sock, addr

    def handle_error(self, request, client_address) -> None:
        """Suppress unhandled traceback dumps to sys.stderr on client disconnects."""
        import sys
        exctype, value, _ = sys.exc_info()
        if exctype in (BrokenPipeError, ConnectionResetError, OSError):
            logger.info("Client %s disconnected: %s", client_address, value)
        else:
            logger.warning("Server error processing request from %s: %s", client_address, value)

    def close_request(self, request):
        """Untrack client connection socket when closed."""
        with self._socket_lock:
            self._active_sockets.discard(request)
        super().close_request(request)

    def close_all_client_sockets(self) -> None:
        """Immediately abort all active streaming TCP connections to stereos."""
        with self._socket_lock:
            sockets = list(self._active_sockets)
            self._active_sockets.clear()
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
                sock.close()
            except Exception:
                pass

    def server_close(self) -> None:
        """Close listening socket and all active client connections."""
        self.close_all_client_sockets()
        super().server_close()

    def add_metadata(self, metadata: AudioMetadata) -> None:
        """Register audio file metadata."""
        self.metadata = metadata
        self.files_by_name[metadata.filename] = metadata

    def get_file_metadata(self, filename: str) -> Optional[AudioMetadata]:
        """Look up metadata for a file, fallback to current metadata."""
        if filename in self.files_by_name:
            return self.files_by_name[filename]
        # Also try case-insensitive or unquoted matching
        for k, v in self.files_by_name.items():
            if k.lower() == filename.lower():
                return v
        return self.metadata


class StreamServerManager:
    """Manages the background execution of the DLNA HTTP Streaming server."""

    def __init__(
        self,
        metadata: Optional[AudioMetadata] = None,
        host_ip: str = "127.0.0.1",
        preferred_port: int = 0,
    ):
        self.metadata = metadata
        self.host_ip = host_ip
        self.preferred_port = preferred_port
        self.server: Optional[DLNAStreamServer] = None
        self.server_thread: Optional[threading.Thread] = None
        self.assigned_port: int = 0

    def start(self, initial_metadata: Optional[AudioMetadata] = None) -> str:
        """Start streaming server in background thread and return streaming URL if metadata provided."""
        if initial_metadata:
            self.metadata = initial_metadata

        ports_to_try = [self.preferred_port] if self.preferred_port != 0 else [8090, 8091, 8092, 8093, 0]

        for port in ports_to_try:
            try:
                self.server = DLNAStreamServer(("0.0.0.0", port), self.metadata)
                self.assigned_port = self.server.server_address[1]
                break
            except OSError:
                if port == 0:
                    raise
                continue

        logger.info("HTTP Streaming Server listening on 0.0.0.0:%d (endpoint: %s)", self.assigned_port, self.host_ip)

        self.server_thread = threading.Thread(
            target=self.server.serve_forever,
            daemon=True,
            name="QuickstreamHTTPServer",
        )
        self.server_thread.start()

        if self.metadata:
            return self.get_stream_url(self.metadata)
        return f"http://{self.host_ip}:{self.assigned_port}/"

    def set_host_ip(self, host_ip: str) -> None:
        """Update host IP for streaming URL generation."""
        self.host_ip = host_ip

    def get_stream_url(self, metadata: AudioMetadata, offset_seconds: float = 0.0) -> str:
        """Register metadata and build stream URL with optional seek offset."""
        if self.server:
            self.server.add_metadata(metadata)
        self.metadata = metadata
        encoded_filename = urllib.parse.quote(metadata.filename)
        base_url = f"http://{self.host_ip}:{self.assigned_port}/stream/{encoded_filename}"
        if offset_seconds > 0.05:
            return f"{base_url}?pos={offset_seconds:.2f}"
        return base_url

    def get_last_client_ip(self) -> str:
        """Return the IP address of the last client that connected to the server."""
        if self.server:
            return self.server.last_client_ip
        return ""

    def get_bytes_streamed(self) -> int:
        """Return total bytes streamed since server started."""
        if self.server:
            return self.server.total_bytes_sent
        return 0

    def stop(self) -> None:
        """Shutdown and terminate the HTTP streaming server."""
        if self.server:
            try:
                self.server.close_all_client_sockets()
                self.server.shutdown()
                self.server.server_close()
            except Exception:
                pass
            self.server = None
        if self.server_thread and self.server_thread.is_alive():
            self.server_thread.join(timeout=1.0)
            self.server_thread = None
