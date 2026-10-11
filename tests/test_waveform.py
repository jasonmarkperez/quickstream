import os
import tempfile
import time
import unittest
import subprocess
from pathlib import Path
import logging

logging.basicConfig(level=logging.DEBUG)

from quickstream.waveform import WaveformExtractor

class TestWaveform(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.test_file = Path(self.temp_dir.name) / "test.mp3"
        
        # Generate a 5-second sine wave using ffmpeg (fast generation)
        subprocess.run([
            "ffmpeg", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
            str(self.test_file)
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        
    def tearDown(self):
        self.temp_dir.cleanup()
        
    def test_waveform_extraction(self):
        extractor = WaveformExtractor()
        results = []
        
        def on_complete(amps):
            results.append(amps)
            
        extractor.extract_async(str(self.test_file), on_complete)
        
        # Wait up to 3 seconds for it to finish
        for _ in range(30):
            if results:
                break
            time.sleep(0.1)
            
        self.assertTrue(len(results) > 0, "Extraction callback not called")
        amps = results[0]
        # 5 seconds at 8000 Hz = ~40000 samples
        self.assertTrue(39000 <= len(amps) <= 41000, f"Expected ~40000 samples, got {len(amps)}")
        
        # Values should be signed 8-bit (-128 to 127)
        for a in amps:
            self.assertTrue(-128 <= a <= 127)
            
        # Ensure it actually detected sound, not just silence
        self.assertTrue(max([abs(a) for a in amps]) > 10, "Waveform max amplitude too low (is it capturing silence?)")
            
        # Test cache
        results.clear()
        extractor.extract_async(str(self.test_file), on_complete)
        self.assertTrue(len(results) > 0, "Cache callback not called immediately")
        
if __name__ == "__main__":
    unittest.main()
