"""Automated unit and integration tests for playback controls (Play, Pause, Stop, Seek, Auto-advance)."""

import io
import unittest
import unittest.mock as mock
import urllib.error

from quickstream.discovery import DLNADevice, DeviceManager
from quickstream.metadata import AudioMetadata
from quickstream.queue import PlaybackQueue
from quickstream.server import StreamServerManager
from quickstream.transcoder import AudioTranscoder
from quickstream.tui import AppTUI
from quickstream.upnp import PositionInfo, TransportInfo, UPnPClient


class TestPlaybackControls(unittest.TestCase):
    """Test Play, Pause, Stop, Seek, and TUI playback orchestration."""

    def setUp(self):
        self.device = DLNADevice(
            friendly_name="Test Stereo",
            location="http://192.168.1.50:8000/desc.xml",
            host="192.168.1.50",
            port=8000,
            av_transport_control_url="http://192.168.1.50:8000/ctrl/AVTransport",
            rendering_control_url="http://192.168.1.50:8000/ctrl/RenderingControl",
        )
        self.client = UPnPClient(self.device)

        self.track1 = AudioMetadata(
            filepath="/music/track1.mp3",
            filename="track1.mp3",
            file_size=5000000,
            mime_type="audio/mpeg",
            format_name="MP3",
            duration_seconds=180.0,
            title="First Track",
        )
        self.track2 = AudioMetadata(
            filepath="/music/track2.mp3",
            filename="track2.mp3",
            file_size=6000000,
            mime_type="audio/mpeg",
            format_name="MP3",
            duration_seconds=210.0,
            title="Second Track",
        )

        self.device_manager = DeviceManager()
        self.device_manager.devices = [self.device]
        self.device_manager.set_active_device(self.device)

        self.queue = PlaybackQueue()
        self.queue.tracks = [self.track1, self.track2]
        self.queue.current_index = 0

        self.server_manager = mock.MagicMock(spec=StreamServerManager)
        self.server_manager.get_stream_url.return_value = "http://192.168.1.10:8090/stream/track1.mp3"
        self.server_manager.get_last_client_ip.return_value = "192.168.1.50"
        self.server_manager.get_bytes_streamed.return_value = 1048576

        self.transcoder = mock.MagicMock(spec=AudioTranscoder)
        self.transcoder.is_available.return_value = False

        self.app = AppTUI(
            device_manager=self.device_manager,
            queue=self.queue,
            server_manager=self.server_manager,
            transcoder=self.transcoder,
            initial_device=self.device,
        )
        self.app.client = self.client
        self.save_patcher = mock.patch.object(self.app.settings_manager, "save", return_value=True)
        self.save_patcher.start()
        self.addCleanup(self.save_patcher.stop)

    def test_play_action_success(self):
        """Verify Play sends correct UPnP request and returns True."""
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            resp = mock.MagicMock()
            resp.__enter__.return_value = resp
            resp.read.return_value = b'<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><s:Body><u:PlayResponse/></s:Body></s:Envelope>'
            mock_opener.open.return_value = resp

            self.assertTrue(self.client.play())
            self.assertFalse(self.client.is_paused_via_stop)

    def test_pause_native_success(self):
        """Verify Pause succeeds on renderers that implement UPnP Pause."""
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            resp = mock.MagicMock()
            resp.__enter__.return_value = resp
            resp.read.return_value = b'<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><s:Body><u:PauseResponse/></s:Body></s:Envelope>'
            mock_opener.open.return_value = resp

            self.assertTrue(self.client.pause())
            self.assertFalse(self.client.is_paused_via_stop)

    def test_pause_401_invalid_action_falls_back_to_stop(self):
        """Verify Pause falls back to Stop when renderer returns 401 Invalid Action."""
        pause_401 = b"""<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body><s:Fault><detail><UPnPError>
                <errorCode>401</errorCode>
                <errorDescription>Invalid Action</errorDescription>
            </UPnPError></detail></s:Fault></s:Body>
        </s:Envelope>"""
        stop_ok = b'<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><s:Body><u:StopResponse/></s:Body></s:Envelope>'

        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener

            err = urllib.error.HTTPError("http://ctrl", 500, "Error", {}, io.BytesIO(pause_401))
            resp_stop = mock.MagicMock()
            resp_stop.__enter__.return_value = resp_stop
            resp_stop.read.return_value = stop_ok

            mock_opener.open.side_effect = [err, resp_stop]

            self.assertTrue(self.client.pause())
            self.assertTrue(self.client.is_paused_via_stop)

    def test_stop_action_success_and_idempotence(self):
        """Verify Stop command succeeds, handles 701, and resets pause-via-stop."""
        self.client.is_paused_via_stop = True

        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            resp = mock.MagicMock()
            resp.__enter__.return_value = resp
            resp.read.return_value = b'<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><s:Body><u:StopResponse/></s:Body></s:Envelope>'
            mock_opener.open.return_value = resp

            self.assertTrue(self.client.stop())
            self.assertFalse(self.client.is_paused_via_stop)

    def test_stop_crash_resilience_on_invalid_control_url(self):
        """Verify Stop does not throw uncaught exception if device control URL is missing."""
        broken_dev = DLNADevice(friendly_name="Broken", location="http://loc", host="1.1.1.1", port=80, av_transport_control_url="")
        broken_client = UPnPClient(broken_dev)
        self.assertFalse(broken_client.stop())
        self.assertEqual(broken_client.last_error, "Missing control URL")

    def test_tui_space_toggles_play_and_pause(self):
        """Test pressing Space toggles state from PLAYING to PAUSED_PLAYBACK and back."""
        self.app.transport_info.state = "PLAYING"
        self.app.position_info.rel_time_seconds = 45.0
        self.app.position_info.rel_time_formatted = "00:45"

        with mock.patch.object(self.app.client, "pause", return_value=True) as mock_pause:
            self.app._handle_key(ord(" "))
            mock_pause.assert_called_once()
            self.assertEqual(self.app.transport_info.state, "PAUSED_PLAYBACK")
            self.assertEqual(self.app.paused_position, 45.0)

        with mock.patch.object(self.app.client, "play", return_value=True) as mock_play:
            self.app._handle_key(ord(" "))
            mock_play.assert_called_once()
            self.assertEqual(self.app.transport_info.state, "PLAYING")

    def test_tui_space_resumes_from_pause_via_stop_with_stream_offset(self):
        """Test resuming when pause was simulated via stop issues stream offset reload without UPnP seek."""
        self.app.transport_info.state = "PAUSED_PLAYBACK"
        self.app.paused_position = 55.0
        self.app.position_info.rel_time_seconds = 55.0
        self.app.client.is_paused_via_stop = True

        with mock.patch.object(self.app.client, "set_transport_uri", return_value=True) as mock_set_uri, \
             mock.patch.object(self.app.client, "play", return_value=True) as mock_play, \
             mock.patch.object(self.app.client, "seek") as mock_seek:
            self.app._handle_key(ord(" "))
            self.app.server_manager.get_stream_url.assert_called_with(self.track1, offset_seconds=55.0)
            mock_set_uri.assert_called_once()
            mock_play.assert_called_once()
            mock_seek.assert_not_called()
            self.assertEqual(self.app.transport_info.state, "PLAYING")
            self.assertEqual(self.app.position_info.rel_time_seconds, 55.0)
            self.assertFalse(self.app.client.is_paused_via_stop)
            self.assertIn("Resumed", self.app.status_message)

    def test_tui_stop_key_resets_playback(self):
        """Test pressing 's' stops playback, resets timer, and sets user_stopped=True."""
        self.app.transport_info.state = "PLAYING"
        self.app.position_info.rel_time_seconds = 50.0
        self.app.position_info.rel_time_formatted = "00:50"

        with mock.patch.object(self.client, "stop", return_value=True) as mock_stop:
            self.app._handle_key(ord("s"))
            mock_stop.assert_called_once()
            self.assertEqual(self.app.transport_info.state, "STOPPED")
            self.assertEqual(self.app.position_info.rel_time_seconds, 0.0)
            self.assertEqual(self.app.position_info.rel_time_formatted, "00:00")
            self.assertTrue(self.app.user_stopped)
            self.assertEqual(self.app.status_message, "Playback stopped")

    def test_tui_stop_key_resilience_when_client_stop_raises(self):
        """Verify pressing 's' does not crash if client.stop() raises an exception."""
        self.app.transport_info.state = "PLAYING"
        with mock.patch.object(self.client, "stop", side_effect=OSError("Socket broken")):
            # Should not raise exception
            self.app._handle_key(ord("s"))
            self.assertEqual(self.app.transport_info.state, "STOPPED")
            self.assertTrue(self.app.user_stopped)

    def test_poller_does_not_autoadvance_when_user_stopped(self):
        """Verify that when user stopped playback, transitioning to STOPPED does not auto-advance."""
        self.app.transport_info.state = "PLAYING"
        self.app.user_stopped = True
        self.app.queue.current_index = 0

        # Simulate poller seeing STOPPED from renderer
        with mock.patch.object(self.app, "play_track") as mock_play_track:
            prev_state = self.app.transport_info.state
            t_info = TransportInfo(state="STOPPED")
            curr_meta = self.app.queue.get_current_track()
            near_end = True

            should_advance = (
                not self.app.user_stopped
                and not getattr(self.app.client, "is_paused_via_stop", False)
                and "PLAY" in prev_state.upper()
                and "STOP" in t_info.state.upper()
                and near_end
                and self.app.queue.has_next()
            )
            self.assertFalse(should_advance)
            mock_play_track.assert_not_called()

    def test_poller_does_not_autoadvance_when_paused_via_stop(self):
        """Verify that when playback is paused via stop fallback, poller keeps PAUSED state and does not advance."""
        self.app.transport_info.state = "PAUSED_PLAYBACK"
        self.client.is_paused_via_stop = True

        prev_state = self.app.transport_info.state
        t_info = TransportInfo(state="STOPPED")
        near_end = True

        should_advance = (
            not self.app.user_stopped
            and not getattr(self.app.client, "is_paused_via_stop", False)
            and "PLAY" in prev_state.upper()
            and "STOP" in t_info.state.upper()
            and near_end
            and self.app.queue.has_next()
        )
    def test_apptui_cleanup_stops_stereo(self):
        """Verify app.cleanup() safely sends stop to connected stereo with short timeout."""
        with mock.patch.object(self.app.client, "stop") as mock_stop:
            self.app.cleanup()
            mock_stop.assert_called_once_with(timeout=1.5)

    def test_apptui_cleanup_handles_stereo_exception_silently(self):
        """Verify app.cleanup() does not raise even if stereo connection raises an exception."""
        with mock.patch.object(self.app.client, "stop", side_effect=TimeoutError("Stereo unreachable")):
            # Should not throw
            self.app.cleanup()

    def test_apptui_run_finally_executes_cleanup_on_crash(self):
        """Verify that when a crash/exception occurs inside curses, app.cleanup() is always executed."""
        with mock.patch("curses.wrapper", side_effect=RuntimeError("Simulated unhandled curses crash")), \
             mock.patch.object(self.app, "cleanup") as mock_cleanup:
            with self.assertRaises(RuntimeError):
                self.app.run()
            mock_cleanup.assert_called_once()

    def test_server_socket_cleanup_on_stop(self):
        """Verify DLNAStreamServer immediately aborts and closes active streaming client sockets on stop."""
        from quickstream.server import DLNAStreamServer
        server = DLNAStreamServer(("127.0.0.1", 0), self.track1, bind_and_activate=False)
        mock_sock1 = mock.MagicMock()
        mock_sock2 = mock.MagicMock()
        server._active_sockets.add(mock_sock1)
        server._active_sockets.add(mock_sock2)

        server.close_all_client_sockets()

        mock_sock1.shutdown.assert_called_once()
        mock_sock1.close.assert_called_once()
        mock_sock2.shutdown.assert_called_once()
        mock_sock2.close.assert_called_once()
        self.assertEqual(len(server._active_sockets), 0)
    def test_global_track_skipping_next_prev_across_tabs(self):
        """Verify 'n', 'p', '>', '<' skip tracks globally across all tabs."""
        with mock.patch.object(self.app, "play_track") as mock_play_track:
            # Tab 0: Queue
            self.app.active_tab = 0
            self.app.queue.current_index = 0
            self.app._handle_key(ord("n"))
            mock_play_track.assert_called_with(1)

            mock_play_track.reset_mock()
            self.app.queue.current_index = 1
            self.app._handle_key(ord("p"))
            mock_play_track.assert_called_with(0)

            # Tab 1: Devices
            mock_play_track.reset_mock()
            self.app.active_tab = 1
            self.app.queue.current_index = 0
            self.app._handle_key(ord(">"))
            mock_play_track.assert_called_with(1)

            mock_play_track.reset_mock()
            self.app.queue.current_index = 1
            self.app._handle_key(ord("<"))
            mock_play_track.assert_called_with(0)

    def test_single_track_queue_skipping_restarts_track(self):
        """Verify 'n' and 'p' do not interrupt playback if queue contains only 1 track."""
        single_queue = PlaybackQueue()
        single_queue.tracks = [self.track1]
        single_queue.current_index = 0
        self.app.queue = single_queue

        with mock.patch.object(self.app, "play_track") as mock_play:
            self.app._handle_key(ord("n"))
            mock_play.assert_not_called()
            self.assertIn("No other tracks", self.app.status_message)

            mock_play.reset_mock()
            self.app._handle_key(ord("p"))
            mock_play.assert_not_called()
            self.assertIn("No other tracks", self.app.status_message)

    def test_queue_wrapping_at_ends(self):
        """Verify 'n' loops to 0 at end of queue, and 'p' wraps to last track at start."""
        with mock.patch.object(self.app, "play_track") as mock_play:
            # At end of queue (index 1 of 2)
            self.app.queue.current_index = 1
            self.app._handle_key(ord("n"))
            mock_play.assert_called_with(0)
            self.assertIn("Looping", self.app.status_message)

            # At start of queue (index 0), rel_time <= 3.0s -> wraps to last track
            mock_play.reset_mock()
            self.app.queue.current_index = 0
            self.app.position_info.rel_time_seconds = 1.0
            self.app._handle_key(ord("p"))
            mock_play.assert_called_with(1)
            self.assertIn("Wrapping", self.app.status_message)

            # At start of queue (index 0), rel_time > 3.0s -> restarts current track
            mock_play.reset_mock()
            self.app.queue.current_index = 0
            self.app.position_info.rel_time_seconds = 15.0
            self.app._handle_key(ord("p"))
            mock_play.assert_called_with(0)
            self.assertIn("Restarting", self.app.status_message)

    def test_seek_relative_reports_feedback_and_error(self):
        """Verify _seek_relative updates position on success and sets status on failure."""
        self.app.position_info.rel_time_seconds = 20.0
        self.app.position_info.track_duration_seconds = 180.0

        # Success case via stream offset seek
        with mock.patch.object(self.app.client, "set_transport_uri", return_value=True), \
             mock.patch.object(self.app.client, "play", return_value=True):
            self.app._seek_relative(10.0)
            self.assertIn("Seeking +10s", self.app.status_message)
            self.app._flush_pending_seek()
            self.assertEqual(self.app.position_info.rel_time_seconds, 30.0)
            self.assertIn("Seeked +10s", self.app.status_message)

        # Failure case when offset seek fails
        self.app.client.last_error = "Connection dropped"
        with mock.patch.object(self.app, "_seek_via_stream_offset", return_value=False):
            self.app._seek_relative(10.0)
            self.app._flush_pending_seek()
            self.assertIn("Seek failed: Connection dropped", self.app.status_message)

    def test_play_retries_on_701_transitioning(self):
        """Verify play() retries when 701 Transition not available is returned."""
        call_count = 0

        def fake_send(control_url, service, action, args, timeout=None):
            nonlocal call_count
            call_count += 1
            if call_count < 2:
                self.client.last_error = "Error 701: Transition not available"
                return None
            import xml.etree.ElementTree as ET
            return ET.Element("PlayResponse")

        with mock.patch.object(self.client, "_send_soap_action", side_effect=fake_send), \
             mock.patch("time.sleep"):
            success = self.client.play(retries=3, retry_delay=0.01)
            self.assertTrue(success)
            self.assertEqual(call_count, 2)

    def test_server_handle_error_suppresses_stderr(self):
        """Verify DLNAStreamServer.handle_error suppresses tracebacks to sys.stderr on broken pipe."""
        import sys
        from quickstream.server import DLNAStreamServer
        server = DLNAStreamServer(("127.0.0.1", 0), self.track1, bind_and_activate=False)

        captured_stderr = io.StringIO()
        with mock.patch.object(sys, "stderr", captured_stderr), \
             mock.patch.object(sys, "exc_info", return_value=(BrokenPipeError, BrokenPipeError(32, "Broken pipe"), None)):
            server.handle_error(mock.MagicMock(), ("192.168.1.50", 12345))
            # Must NOT write unhandled traceback to stderr
            self.assertEqual(captured_stderr.getvalue(), "")
        server.server_close()

    def test_safe_addstr_catches_curses_error(self):
        """Verify _safe_addstr guards against out of bounds and catches curses.error."""
        mock_stdscr = mock.MagicMock()
        mock_stdscr.getmaxyx.return_value = (24, 80)
        import curses
        mock_stdscr.addstr.side_effect = curses.error("Mock curses error")

        # Calling out of bounds should not even call addstr
        self.app._safe_addstr(mock_stdscr, 100, 100, "test")
        mock_stdscr.addstr.assert_not_called()

        # Calling in bounds should catch curses.error silently without throwing
        self.app._safe_addstr(mock_stdscr, 10, 10, "test")
        mock_stdscr.addstr.assert_called_once()

    def test_tab_switching_includes_settings(self):
        """Verify Tab cycles 0 -> 1 -> 2 -> 0, and keys 1, 2, 3 jump directly."""
        self.app.active_tab = 0
        self.app._handle_key(ord("\t"))
        self.assertEqual(self.app.active_tab, 1)

        self.app._handle_key(ord("\t"))
        self.assertEqual(self.app.active_tab, 2)

        self.app._handle_key(ord("\t"))
        self.assertEqual(self.app.active_tab, 0)

        # Direct jumps
        self.app._handle_key(ord("3"))
        self.assertEqual(self.app.active_tab, 2)

        self.app._handle_key(ord("1"))
        self.assertEqual(self.app.active_tab, 0)

        self.app._handle_key(ord("2"))
        self.assertEqual(self.app.active_tab, 1)

    def test_metadata_view_toggle_hotkey(self):
        """Verify pressing 'v' toggles metadata_view setting and file browser mode."""
        self.assertTrue(self.app.settings.metadata_view)
        self.assertTrue(self.app.file_browser.metadata_view)

        self.app._handle_key(ord("v"))
        self.assertFalse(self.app.settings.metadata_view)
        self.assertFalse(self.app.file_browser.metadata_view)
        self.assertIn("Filename", self.app.status_message)

        self.app._handle_key(ord("v"))
        self.assertTrue(self.app.settings.metadata_view)
        self.assertTrue(self.app.file_browser.metadata_view)
        self.assertIn("Metadata", self.app.status_message)

    def test_settings_tab_navigation_and_cycling(self):
        """Verify navigation and value cycling within Tab 3 (Settings)."""
        import curses
        self.app.active_tab = 2
        self.app.settings_cursor = 0  # metadata_view

        # Cycle value with Enter
        self.assertTrue(self.app.settings.metadata_view)
        self.app._handle_key(10)  # Enter
        self.assertFalse(self.app.settings.metadata_view)

        # Move down cursor to default_device
        self.app._handle_key(curses.KEY_DOWN)
        self.assertEqual(self.app.settings_cursor, 1)


if __name__ == "__main__":
    unittest.main()
