"""Automated unit tests for LocalPlayerClient (ffplay-based local audio playback)."""

import unittest
from unittest import mock

from quickstream.discovery import DLNADevice
from quickstream.local_player import LocalPlayerClient
from quickstream.metadata import AudioMetadata


class TestLocalPlayerClient(unittest.TestCase):
    """Test LocalPlayerClient playback lifecycle, seeking, volume, and state tracking."""

    def setUp(self):
        self.device = DLNADevice(
            friendly_name="Local Audio",
            location="localhost",
            host="localhost",
            port=0,
            manufacturer="Quickstream",
            udn="local-audio",
        )
        self.client = LocalPlayerClient(self.device)
        self.metadata = AudioMetadata(
            filepath="/music/test.flac",
            filename="test.flac",
            file_size=15000000,
            mime_type="audio/flac",
            format_name="FLAC",
            duration_seconds=200.0,
            title="Local Flac Song",
            artist="Artist Name",
        )

    def test_init_and_set_transport_uri(self):
        """Verify initial state and transport URI setup."""
        self.assertEqual(self.client.state, "STOPPED")
        self.assertEqual(self.client.volume, 50)
        self.assertFalse(self.client.muted)

        result = self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        self.assertTrue(result)
        self.assertEqual(self.client.current_file, "/music/test.flac")
        self.assertEqual(self.client.duration, 200.0)
        self.assertEqual(self.client.offset, 0.0)

    @mock.patch("shutil.which", return_value=None)
    def test_play_fails_when_ffplay_missing(self, mock_which):
        """Verify play returns False and records error if ffplay is not found in PATH."""
        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        success = self.client.play()
        self.assertFalse(success)
        self.assertIn("ffplay not found", self.client.last_error)

    @mock.patch("subprocess.Popen")
    @mock.patch("shutil.which", return_value="/usr/bin/ffplay")
    def test_play_success_spawns_process(self, mock_which, mock_popen):
        """Verify play spawns ffplay with proper arguments."""
        mock_proc = mock.MagicMock()
        mock_proc.poll.return_value = None
        mock_popen.return_value = mock_proc

        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        success = self.client.play()

        self.assertTrue(success)
        self.assertEqual(self.client.state, "PLAYING")
        mock_popen.assert_called_once()
        cmd = mock_popen.call_args[0][0]
        self.assertEqual(cmd[0], "/usr/bin/ffplay")
        self.assertIn("-nodisp", cmd)
        self.assertIn("-autoexit", cmd)
        self.assertIn("-volume", cmd)
        self.assertEqual(cmd[cmd.index("-volume") + 1], "50")
        self.assertEqual(cmd[-1], "/music/test.flac")

    @mock.patch("subprocess.Popen")
    @mock.patch("shutil.which", return_value="/usr/bin/ffplay")
    def test_pause_and_resume(self, mock_which, mock_popen):
        """Verify pause terminates active process and saves playback offset."""
        mock_proc = mock.MagicMock()
        mock_proc.poll.return_value = None
        mock_popen.return_value = mock_proc

        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        self.client.play()
        self.client.start_time = 100.0

        with mock.patch("time.time", return_value=125.0):
            paused = self.client.pause()
            self.assertTrue(paused)
            self.assertEqual(self.client.state, "PAUSED_PLAYBACK")
            self.assertAlmostEqual(self.client.offset, 25.0)
            mock_proc.terminate.assert_called_once()
            self.assertIsNone(self.client.proc)

        # Resume playback from paused offset
        mock_proc2 = mock.MagicMock()
        mock_proc2.poll.return_value = None
        mock_popen.return_value = mock_proc2

        self.client.play()
        self.assertEqual(self.client.state, "PLAYING")
        cmd = mock_popen.call_args[0][0]
        self.assertIn("-ss", cmd)
        self.assertIn("25.0", cmd)

    @mock.patch("subprocess.Popen")
    @mock.patch("shutil.which", return_value="/usr/bin/ffplay")
    def test_stop_resets_state_and_offset(self, mock_which, mock_popen):
        """Verify stop terminates process and resets offset to 0."""
        mock_proc = mock.MagicMock()
        mock_proc.poll.return_value = None
        mock_popen.return_value = mock_proc

        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        self.client.play()
        self.client.offset = 45.0

        stopped = self.client.stop()
        self.assertTrue(stopped)
        self.assertEqual(self.client.state, "STOPPED")
        self.assertEqual(self.client.offset, 0.0)
        self.assertIsNone(self.client.proc)
        mock_proc.terminate.assert_called_once()

    @mock.patch("subprocess.Popen")
    @mock.patch("shutil.which", return_value="/usr/bin/ffplay")
    def test_seek_while_playing_restarts_process_with_offset(self, mock_which, mock_popen):
        """Verify seek during playback restarts process at the target seconds."""
        mock_proc = mock.MagicMock()
        mock_proc.poll.return_value = None
        mock_popen.return_value = mock_proc

        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        self.client.play()

        seeked = self.client.seek(75.0)
        self.assertTrue(seeked)
        self.assertEqual(self.client.offset, 75.0)
        cmd = mock_popen.call_args[0][0]
        self.assertIn("-ss", cmd)
        self.assertIn("75.0", cmd)

    def test_seek_while_paused_updates_offset_without_spawning(self):
        """Verify seek while paused only updates offset and does not spawn process."""
        self.client.set_transport_uri(self.metadata.filepath, self.metadata)
        self.client.state = "PAUSED_PLAYBACK"
        seeked = self.client.seek(50.0)
        self.assertTrue(seeked)
        self.assertEqual(self.client.offset, 50.0)
        self.assertIsNone(self.client.proc)

    def test_volume_and_mute_controls(self):
        """Verify volume clamping and mute controls."""
        self.client.set_volume(80)
        self.assertEqual(self.client.get_volume(), 80)

        # Clamping
        self.client.set_volume(150)
        self.assertEqual(self.client.get_volume(), 100)
        self.client.set_volume(-10)
        self.assertEqual(self.client.get_volume(), 0)

        self.client.set_mute(True)
        self.assertTrue(self.client.get_mute())
        self.client.set_mute(False)
        self.assertFalse(self.client.get_mute())

    def test_get_transport_info_detects_natural_exit(self):
        """Verify get_transport_info updates state to STOPPED when process has finished."""
        mock_proc = mock.MagicMock()
        mock_proc.poll.return_value = 0  # exited
        self.client.proc = mock_proc
        self.client.state = "PLAYING"

        info = self.client.get_transport_info()
        self.assertEqual(info.state, "STOPPED")
        self.assertIsNone(self.client.proc)

    def test_get_position_info_formatting(self):
        """Verify position information formatting for duration and relative time."""
        self.client.duration = 245.0  # 4m 5s
        self.client.state = "PLAYING"
        self.client.start_time = 100.0

        with mock.patch("time.time", return_value=175.0):  # 75s elapsed (1m 15s)
            pos = self.client.get_position_info()
            self.assertEqual(pos.rel_time_seconds, 75.0)
            self.assertEqual(pos.rel_time_formatted, "01:15")
            self.assertEqual(pos.track_duration_seconds, 245.0)
            self.assertEqual(pos.track_duration_formatted, "04:05")


if __name__ == "__main__":
    unittest.main()
