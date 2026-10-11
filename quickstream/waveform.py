import logging
import subprocess
import shutil
import threading
from typing import Callable, Optional

logger = logging.getLogger("quickstream.waveform")


class WaveformExtractor:
    """Asynchronously extracts a lightweight audio amplitude envelope."""

    def __init__(self):
        import array
        self.cache: dict[str, array.array] = {}
        self._current_thread: Optional[threading.Thread] = None
        self.ffmpeg_path = shutil.which("ffmpeg")

    def extract_async(self, filepath: str, on_complete: Callable[['array.array'], None]) -> None:
        """Runs ffmpeg in a background thread to extract a 20Hz amplitude envelope."""
        if not self.ffmpeg_path:
            logger.warning("ffmpeg not found in PATH")
            return

        if filepath in self.cache:
            on_complete(self.cache[filepath])
            return

        def _worker():
            logger.info("Starting waveform extraction for: %s", filepath)
            try:
                # Extract to 8000Hz mono, 8-bit signed PCM for high resolution
                cmd = [
                    self.ffmpeg_path, "-y", "-i", filepath,
                    "-ac", "1", "-ar", "8000",
                    "-f", "s8", "-c:a", "pcm_s8", "-"
                ]
                proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
                if proc.returncode == 0 and proc.stdout:
                    import array
                    amplitudes = array.array('b', proc.stdout)
                    self.cache[filepath] = amplitudes
                    logger.info("Waveform extraction successful: %d samples", len(amplitudes))
                    on_complete(amplitudes)
                else:
                    logger.error("Waveform extraction failed. code: %s, stderr: %s", proc.returncode, proc.stderr.decode('utf-8', errors='ignore'))
            except Exception as e:
                logger.error("Waveform extraction exception: %s", e)

        self._current_thread = threading.Thread(target=_worker, daemon=True)
        self._current_thread.start()
