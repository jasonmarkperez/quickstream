"""Unit tests for UPnP device description parsing."""

import unittest

from quickstream.discovery import parse_device_description


class TestDiscovery(unittest.TestCase):
    """Test device XML description parsing."""

    def test_parse_valid_device_description(self):
        """Test parsing valid UPnP MediaRenderer XML description."""
        sample_xml = """<?xml version="1.0"?>
        <root xmlns="urn:schemas-upnp-org:device-1-0">
            <device>
                <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
                <friendlyName>Denon AVR-X2700H</friendlyName>
                <manufacturer>Denon</manufacturer>
                <modelName>AVR-X2700H</modelName>
                <UDN>uuid:12345678-abcd-ef01-2345-6789abcdef01</UDN>
                <serviceList>
                    <service>
                        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
                        <controlURL>/AVTransport/control</controlURL>
                        <eventSubURL>/AVTransport/event</eventSubURL>
                        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
                    </service>
                    <service>
                        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
                        <controlURL>/RenderingControl/control</controlURL>
                        <eventSubURL>/RenderingControl/event</eventSubURL>
                        <SCPDURL>/RenderingControl/scpd.xml</SCPDURL>
                    </service>
                </serviceList>
            </device>
        </root>
        """
        location_url = "http://192.168.1.50:8080/description.xml"
        device = parse_device_description(sample_xml, location_url)

        self.assertIsNotNone(device)
        self.assertEqual(device.friendly_name, "Denon AVR-X2700H")
        self.assertEqual(device.host, "192.168.1.50")
        self.assertEqual(device.port, 8080)
        self.assertEqual(device.manufacturer, "Denon")
        self.assertEqual(device.model_name, "AVR-X2700H")
        self.assertEqual(device.udn, "uuid:12345678-abcd-ef01-2345-6789abcdef01")
        self.assertEqual(
            device.av_transport_control_url,
            "http://192.168.1.50:8080/AVTransport/control",
        )
        self.assertEqual(
            device.rendering_control_url,
            "http://192.168.1.50:8080/RenderingControl/control",
        )

    def test_parse_device_without_avtransport(self):
        """Test that devices missing AVTransport service are ignored."""
        sample_xml = """<?xml version="1.0"?>
        <root xmlns="urn:schemas-upnp-org:device-1-0">
            <device>
                <deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType>
                <friendlyName>Smart Router</friendlyName>
                <serviceList>
                    <service>
                        <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
                        <controlURL>/WANIPConnection/control</controlURL>
                    </service>
                </serviceList>
            </device>
        </root>
        """
        location_url = "http://192.168.1.1:80/description.xml"
        device = parse_device_description(sample_xml, location_url)
        self.assertIsNone(device)

    def test_device_manager(self):
        """Test DeviceManager state and device selection."""
        from quickstream.discovery import DLNADevice, DeviceManager
        mgr = DeviceManager()
        self.assertEqual(len(mgr.devices), 1)
        self.assertIsNone(mgr.active_device)

        d1 = DLNADevice(
            friendly_name="Living Room Stereo",
            location="http://192.168.1.10:8080/desc.xml",
            host="192.168.1.10",
            port=8080,
            av_transport_control_url="http://192.168.1.10:8080/AVTransport/ctrl",
        )
        mgr.devices.append(d1)
        mgr.set_active_device(d1)
        self.assertEqual(mgr.active_device.friendly_name, "Living Room Stereo")

    def test_discover_devices_timeout_returns_list(self):
        """Verify discover_devices returns list even when socket times out."""
        from unittest.mock import MagicMock, patch
        from quickstream.discovery import discover_devices
        import socket

        # Mock socket to raise timeout immediately
        with patch("socket.socket") as mock_sock_cls:
            mock_sock = MagicMock()
            mock_sock.recvfrom.side_effect = socket.timeout
            mock_sock_cls.return_value = mock_sock

            devices = discover_devices(timeout=0.05)
            self.assertIsInstance(devices, list)
            self.assertEqual(len(devices), 0)


if __name__ == "__main__":
    unittest.main()
