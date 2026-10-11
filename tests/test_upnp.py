"""Unit tests for UPnP SOAP payloads, DIDL-Lite metadata, and time parsing."""

import unittest
import xml.etree.ElementTree as ET

from quickstream.discovery import DLNADevice
from quickstream.metadata import AudioMetadata
from quickstream.upnp import (
    UPnPClient,
    create_didl_lite_metadata,
    format_seconds_to_upnp_time,
    parse_upnp_time_to_seconds,
)


class TestUPnP(unittest.TestCase):
    """Test UPnP client helper functions and XML builders."""

    def test_time_conversions(self):
        """Test UPnP HH:MM:SS formatting and parsing."""
        self.assertEqual(format_seconds_to_upnp_time(0), "00:00:00")
        self.assertEqual(format_seconds_to_upnp_time(65), "00:01:05")
        self.assertEqual(format_seconds_to_upnp_time(3665), "01:01:05")

        self.assertEqual(parse_upnp_time_to_seconds("00:01:05"), 65.0)
        self.assertEqual(parse_upnp_time_to_seconds("01:01:05"), 3665.0)
        self.assertEqual(parse_upnp_time_to_seconds("00:01:05.500"), 65.5)
        self.assertEqual(parse_upnp_time_to_seconds("NOT_IMPLEMENTED"), 0.0)

    def test_didl_lite_metadata_generation(self):
        """Test DIDL-Lite metadata generation."""
        meta = AudioMetadata(
            filepath="/music/track.flac",
            filename="track.flac",
            file_size=2048000,
            mime_type="audio/flac",
            format_name="FLAC",
            duration_seconds=185.0,
            title="A Nice Song & Melody",
            artist="The Band",
            album="First Record",
        )
        stream_url = "http://192.168.1.10:8080/stream/track.flac"

        didl_xml = create_didl_lite_metadata(stream_url, meta)
        # Parse XML to verify valid syntax
        root = ET.fromstring(didl_xml)
        self.assertIn("DIDL-Lite", root.tag)

        # Verify title and creator exist
        title_found = False
        creator_found = False
        res_found = False

        for elem in root.iter():
            tag = elem.tag.split("}", 1)[1] if "}" in elem.tag else elem.tag
            if tag == "title":
                self.assertEqual(elem.text, "A Nice Song & Melody")
                title_found = True
            elif tag == "creator":
                self.assertEqual(elem.text, "The Band")
                creator_found = True
            elif tag == "res":
                self.assertEqual(elem.text, stream_url)
                self.assertIn("audio/flac", elem.attrib.get("protocolInfo", ""))
                res_found = True

        self.assertTrue(title_found)
        self.assertTrue(creator_found)
        self.assertTrue(res_found)

    def test_response_parsing(self):
        """Test extraction of values from UPnP response XML."""
        xml_data = """
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body>
                <u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
                    <CurrentTransportState>PLAYING</CurrentTransportState>
                    <CurrentTransportStatus>OK</CurrentTransportStatus>
                    <CurrentSpeed>1</CurrentSpeed>
                </u:GetTransportInfoResponse>
            </s:Body>
        </s:Envelope>
        """
        elem = ET.fromstring(xml_data)
        device = DLNADevice(
            friendly_name="Test Stereo",
            location="http://127.0.0.1:8000/desc.xml",
            host="127.0.0.1",
            port=8000,
            av_transport_control_url="http://127.0.0.1:8000/ctrl",
        )
        client = UPnPClient(device)
        self.assertEqual(client._extract_response_value(elem, "CurrentTransportState"), "PLAYING")
        self.assertEqual(client._extract_response_value(elem, "CurrentTransportStatus"), "OK")

    def setUp(self):
        self.test_device = DLNADevice(
            friendly_name="Living Room Stereo",
            location="http://192.168.1.50:8000/desc.xml",
            host="192.168.1.50",
            port=8000,
            av_transport_control_url="http://192.168.1.50:8000/ctrl/AVTransport",
            rendering_control_url="http://192.168.1.50:8000/ctrl/RenderingControl",
        )

    def test_play_success(self):
        """Test Play action sends correct SOAP action and arguments."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b"""<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
                <s:Body><u:PlayResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body>
            </s:Envelope>"""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.play())
            self.assertFalse(client.is_paused_via_stop)

            req = mock_opener.open.call_args[0][0]
            self.assertEqual(req.full_url, "http://192.168.1.50:8000/ctrl/AVTransport")
            self.assertIn("AVTransport:1#Play", req.headers["Soapaction"])

    def test_pause_success(self):
        """Test Pause action when renderer natively supports UPnP Pause."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b"""<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
                <s:Body><u:PauseResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body>
            </s:Envelope>"""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.pause())
            self.assertFalse(client.is_paused_via_stop)

    def test_pause_401_invalid_action_fallback_to_stop(self):
        """Verify that when stereo returns 401 Invalid Action on Pause, it falls back to Stop."""
        import io
        import urllib.error
        import unittest.mock as mock

        pause_fault = b"""<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body>
                <s:Fault>
                    <detail>
                        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
                            <errorCode>401</errorCode>
                            <errorDescription>Invalid Action</errorDescription>
                        </UPnPError>
                    </detail>
                </s:Fault>
            </s:Body>
        </s:Envelope>"""
        stop_success = b"""<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body><u:StopResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body>
        </s:Envelope>"""

        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener

            stop_resp = mock.MagicMock()
            stop_resp.__enter__.return_value = stop_resp
            stop_resp.read.return_value = stop_success
            # First call (Pause) raises 401 fault; second call (Stop fallback) returns 200 OK
            err = urllib.error.HTTPError(
                "http://192.168.1.50:8000/ctrl/AVTransport",
                500,
                "Internal Server Error",
                {},
                io.BytesIO(pause_fault),
            )
            mock_opener.open.side_effect = [err, stop_resp]

            client = UPnPClient(self.test_device)
            result = client.pause()

            self.assertTrue(result)
            self.assertTrue(client.is_paused_via_stop)

    def test_stop_success(self):
        """Test Stop action cleanly succeeds."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b"""<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
                <s:Body><u:StopResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body>
            </s:Envelope>"""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.stop())
            self.assertFalse(client.is_paused_via_stop)

    def test_stop_empty_body_200(self):
        """Some renderers return HTTP 200 OK with empty body on Stop."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.stop())

    def test_stop_701_already_stopped(self):
        """Stereo returning Error 701 (Transition not available) because already stopped."""
        import io
        import urllib.error
        import unittest.mock as mock

        fault_701 = b"""<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body>
                <s:Fault>
                    <detail>
                        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
                            <errorCode>701</errorCode>
                            <errorDescription>Transition not available</errorDescription>
                        </UPnPError>
                    </detail>
                </s:Fault>
            </s:Body>
        </s:Envelope>"""
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            err = urllib.error.HTTPError(
                "http://192.168.1.50:8000/ctrl/AVTransport",
                500,
                "Internal Server Error",
                {},
                io.BytesIO(fault_701),
            )
            mock_opener.open.side_effect = err

            client = UPnPClient(self.test_device)
            self.assertTrue(client.stop())

    def test_stop_missing_control_url(self):
        """Device with missing control URL returns False without unhandled exception."""
        dev = DLNADevice(
            friendly_name="Broken Device",
            location="http://127.0.0.1:8000/desc.xml",
            host="127.0.0.1",
            port=8000,
            av_transport_control_url="",
        )
        client = UPnPClient(dev)
        self.assertFalse(client.stop())
        self.assertEqual(client.last_error, "Missing control URL")

    def test_seek_action(self):
        """Test Seek action with formatted REL_TIME string."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b"""<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
                <s:Body><u:SeekResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body>
            </s:Envelope>"""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.seek(125.0))

            req = mock_opener.open.call_args[0][0]
            self.assertIn("AVTransport:1#Seek", req.headers["Soapaction"])
            body_text = req.data.decode("utf-8")
            self.assertIn("<Target>00:02:05</Target>", body_text)

    def test_rendering_control_volume_and_mute(self):
        """Test volume setting with clamping and mute toggling."""
        import unittest.mock as mock
        with mock.patch("urllib.request.build_opener") as mock_build_opener:
            mock_opener = mock.MagicMock()
            mock_build_opener.return_value = mock_opener
            mock_resp = mock.MagicMock()
            mock_resp.read.return_value = b"""<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
                <s:Body><u:SetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1" /></s:Body>
            </s:Envelope>"""
            mock_opener.open.return_value.__enter__.return_value = mock_resp

            client = UPnPClient(self.test_device)
            self.assertTrue(client.set_volume(45))
            req = mock_opener.open.call_args[0][0]
            self.assertIn("<DesiredVolume>45</DesiredVolume>", req.data.decode("utf-8"))

            # Clamping check: volume > 100 clamped to 100
            self.assertTrue(client.set_volume(150))
            req2 = mock_opener.open.call_args[0][0]
            self.assertIn("<DesiredVolume>100</DesiredVolume>", req2.data.decode("utf-8"))

            # Set Mute
            self.assertTrue(client.set_mute(True))
            req3 = mock_opener.open.call_args[0][0]
            self.assertIn("<DesiredMute>1</DesiredMute>", req3.data.decode("utf-8"))


if __name__ == "__main__":
    unittest.main()
