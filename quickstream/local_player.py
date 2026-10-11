import subprocess
import threading
import time
import logging
from typing import Optional

from quickstream.discovery import DLNADevice
from quickstream.metadata import AudioMetadata
from quickstream.upnp import PositionInfo, TransportInfo, UPnPClient

logger = logging.getLogger("quickstream.local_player")

class LocalPlayerClient:
    """A mock UPnPClient that plays audio locally using ffplay."""
    
    def __init__(self, device: DLNADevice, timeout: float = 4.0):
        self.device = device
        self.timeout = timeout
        self.last_error = ""
        self.is_paused_via_stop = False
        
        self.proc: Optional[subprocess.Popen] = None
        self.current_file = ""
        self.state = "STOPPED"
        self.start_time = 0.0
        self.offset = 0.0
        self.duration = 0.0
        
        self.volume = 50
        self.muted = False

    def set_transport_uri(self, stream_url: str, metadata: AudioMetadata) -> bool:
        self.current_file = metadata.filepath
        self.duration = metadata.duration_seconds
        self.offset = 0.0
        return True

    def _start_ffplay(self, offset: float) -> bool:
        if self.proc:
            self.proc.terminate()
            self.proc.wait()
            
        import shutil
        ffplay_path = shutil.which("ffplay")
        if not ffplay_path:
            self.last_error = "ffplay not found in PATH"
            return False
        
        cmd = [ffplay_path, "-nodisp", "-autoexit"]
        if offset > 0:
            cmd.extend(["-ss", str(offset)])
            
        # ffplay volume is 0-100
        vol = 0 if self.muted else self.volume
        cmd.extend(["-volume", str(vol)])
        
        cmd.append(str(self.current_file))
        try:
            self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.PIPE)
            self.start_time = time.time() - offset
            self.state = "PLAYING"
            return True
        except Exception as e:
            self.last_error = str(e)
            return False

    def play(self, speed: str = "1", retries: int = 3, retry_delay: float = 0.35) -> bool:
        self.is_paused_via_stop = False
        if not self.current_file:
            return False
            
        if self.state == "PLAYING" and self.proc and self.proc.poll() is None:
            return True
            
        return self._start_ffplay(self.offset)

    def pause(self) -> bool:
        if self.proc and self.proc.poll() is None and self.state == "PLAYING":
            self.offset = time.time() - self.start_time
            self.state = "PAUSED_PLAYBACK"
            
            # Terminate gracefully to avoid audio clicks
            try:
                self.proc.terminate()
                self.proc.wait(timeout=1.0)
            except Exception:
                self.proc.kill()
            self.proc = None
            return True
        return False

    def stop(self, timeout: Optional[float] = None) -> bool:
        self.is_paused_via_stop = False
        self.state = "STOPPED"
        if self.proc:
            self.proc.terminate()
            self.proc.wait()
            self.proc = None
        self.offset = 0.0
        return True

    def seek(self, target_seconds: float) -> bool:
        self.offset = target_seconds
        if self.state == "PLAYING":
            return self._start_ffplay(target_seconds)
        elif self.state == "PAUSED_PLAYBACK":
            # If we seek while paused, we don't need to do anything since the process is dead
            pass
        return True

    def get_transport_info(self, timeout: float = 1.5) -> TransportInfo:
        if self.proc:
            if self.proc.poll() is not None:
                self.state = "STOPPED"
                self.proc = None
        return TransportInfo(state=self.state, status="OK", speed="1")

    def get_position_info(self, timeout: float = 1.5) -> PositionInfo:
        rel_time = self.offset
        if self.state == "PLAYING":
            rel_time = time.time() - self.start_time
            
        def fmt(s: float) -> str:
            total = int(s)
            h = total // 3600
            m = (total % 3600) // 60
            sec = total % 60
            if h > 0:
                return f"{h:02d}:{m:02d}:{sec:02d}"
            return f"{m:02d}:{sec:02d}"
            
        return PositionInfo(
            rel_time_seconds=rel_time,
            track_duration_seconds=self.duration,
            rel_time_formatted=fmt(rel_time),
            track_duration_formatted=fmt(self.duration),
            track_uri=self.current_file,
        )

    def get_volume(self, channel: str = "Master", timeout: float = 1.5) -> Optional[int]:
        return self.volume

    def set_volume(self, volume: int, channel: str = "Master") -> bool:
        self.volume = max(0, min(100, volume))
        return True

    def get_mute(self, channel: str = "Master", timeout: float = 1.5) -> Optional[bool]:
        return self.muted

    def set_mute(self, muted: bool, channel: str = "Master") -> bool:
        self.muted = muted
        if self.proc and self.proc.stdin:
            try:
                self.proc.stdin.write(b'm')
                self.proc.stdin.flush()
            except Exception:
                pass
        return True
