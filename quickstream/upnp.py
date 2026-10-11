"""UPnP AVTransport and RenderingControl SOAP Client."""

from dataclasses import dataclass
import html
import logging
import time
from typing import Any, Dict, Optional, Tuple
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

from quickstream.discovery import DLNADevice
from quickstream.metadata import AudioMetadata

logger = logging.getLogger("quickstream.upnp")

AV_TRANSPORT_SERVICE = "urn:schemas-upnp-org:service:AVTransport:1"
RENDERING_CONTROL_SERVICE = "urn:schemas-upnp-org:service:RenderingControl:1"


@dataclass
class PositionInfo:
    """Current playback position information from AVTransport."""

    rel_time_seconds: float = 0.0
    track_duration_seconds: float = 0.0
    rel_time_formatted: str = "00:00"
    track_duration_formatted: str = "00:00"
    track_uri: str = ""


@dataclass
class TransportInfo:
    """Current transport state information from AVTransport."""

    state: str = "STOPPED"  # PLAYING, PAUSED_PLAYBACK, STOPPED, TRANSITIONING, NO_MEDIA_PRESENT
    status: str = "OK"
    speed: str = "1"


def format_seconds_to_upnp_time(seconds: float) -> str:
    """Format seconds into UPnP time string 'HH:MM:SS'."""
    sec = max(0, int(seconds))
    h = sec // 3600
    m = (sec % 3600) // 60
    s = sec % 60
    return f"{h:02d}:{m:02d}:{s:02d}"


def parse_upnp_time_to_seconds(time_str: str) -> float:
    """Parse UPnP time string 'HH:MM:SS' or 'HH:MM:SS.xxx' to seconds."""
    if not time_str or time_str == "NOT_IMPLEMENTED" or time_str == "--:--:--":
        return 0.0
    try:
        parts = time_str.split(":")
        if len(parts) == 3:
            h = int(parts[0])
            m = int(parts[1])
            s = float(parts[2])
            return h * 3600 + m * 60 + s
        elif len(parts) == 2:
            m = int(parts[0])
            s = float(parts[1])
            return m * 60 + s
    except Exception:
        pass
    return 0.0


def create_didl_lite_metadata(
    stream_url: str,
    metadata: AudioMetadata,
) -> str:
    """Generate DIDL-Lite XML metadata for SetAVTransportURI."""
    title = html.escape(metadata.display_title)
    artist = html.escape(metadata.artist if metadata.artist else "Unknown Artist")
    album = html.escape(metadata.album if metadata.album else "")
    mime = metadata.mime_type
    duration = format_seconds_to_upnp_time(metadata.duration_seconds)

    album_xml = f"<upnp:album>{album}</upnp:album>" if album else ""
    duration_attr = f' duration="{duration}"' if metadata.duration_seconds > 0 else ""

    didl = (
        '<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
        'xmlns:dc="http://purl.org/dc/elements/1.1/" '
        'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
        '<item id="0" parentID="-1" restricted="false">'
        f"<dc:title>{title}</dc:title>"
        f"<dc:creator>{artist}</dc:creator>"
        f"{album_xml}"
        f"<upnp:class>object.item.audioItem.musicTrack</upnp:class>"
        f'<res protocolInfo="http-get:*:{mime}:*"{duration_attr}>{stream_url}</res>'
        "</item>"
        "</DIDL-Lite>"
    )
    return didl


class UPnPClient:
    """Controls DLNA MediaRenderer via AVTransport and RenderingControl SOAP calls."""

    def __init__(self, device: DLNADevice, timeout: float = 4.0):
        self.device = device
        self.timeout = timeout
        self.last_error: str = ""
        self.is_paused_via_stop: bool = False

    def _send_soap_action(
        self,
        control_url: str,
        service_type: str,
        action: str,
        arguments: Dict[str, Any],
        timeout: Optional[float] = None,
    ) -> Optional[ET.Element]:
        """Send a UPnP SOAP action and return parsed XML Element."""
        self.last_error = ""
        if not control_url:
            self.last_error = "Missing control URL"
            logger.warning("SOAP request aborted on %s: %s", action, self.last_error)
            return None

        args_xml = "".join(f"<{k}>{html.escape(str(v))}</{k}>" for k, v in arguments.items())
        body = (
            '<?xml version="1.0" encoding="utf-8"?>\r\n'
            '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">\r\n'
            "  <s:Body>\r\n"
            f'    <u:{action} xmlns:u="{service_type}">\r\n'
            f"      {args_xml}\r\n"
            f"    </u:{action}>\r\n"
            "  </s:Body>\r\n"
            "</s:Envelope>"
        )

        headers = {
            "Content-Type": 'text/xml; charset="utf-8"',
            "SOAPAction": f'"{service_type}#{action}"',
            "User-Agent": "Quickstream/0.1.0 UPnP/1.1",
        }

        effective_timeout = timeout if timeout is not None else self.timeout

        try:
            req = urllib.request.Request(
                control_url,
                data=body.encode("utf-8"),
                headers=headers,
                method="POST",
            )
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(req, timeout=effective_timeout) as response:
                content = response.read().decode("utf-8", errors="replace")
                if not content.strip():
                    return ET.Element(f"{action}Response")

                root = ET.fromstring(content)
                # Check for SOAP fault in 200 response
                if self._extract_response_value(root, "Fault") is not None or self._extract_response_value(root, "UPnPError") is not None:
                    code = self._extract_response_value(root, "errorCode")
                    desc = self._extract_response_value(root, "errorDescription")
                    self.last_error = f"Error {code}: {desc}" if code else "SOAP Fault"
                    logger.warning("SOAP fault on %s: %s", action, self.last_error)
                    return None
                return root
        except urllib.error.HTTPError as e:
            content = e.read().decode("utf-8", errors="replace")
            self.last_error = f"HTTP {e.code}"
            try:
                fault_xml = ET.fromstring(content)
                code = self._extract_response_value(fault_xml, "errorCode")
                desc = self._extract_response_value(fault_xml, "errorDescription")
                if code:
                    self.last_error = f"Error {code}: {desc}"
            except Exception:
                pass
            logger.warning("SOAP HTTPError on %s: %s", action, self.last_error)
            return None
        except Exception as e:
            self.last_error = str(e)
            logger.warning("SOAP request error on %s: %s", action, e)
            return None

    def _extract_response_value(self, element: Optional[ET.Element], tag_name: str) -> Optional[str]:
        """Find value of a given tag regardless of XML namespaces."""
        if element is None:
            return None
        for elem in element.iter():
            tag = elem.tag.split("}", 1)[1] if "}" in elem.tag else elem.tag
            if tag == tag_name:
                return elem.text
            # Also check case-insensitive match for tags like ErrorCode / errorCode
            if tag.lower() == tag_name.lower():
                return elem.text
        return None

    # AVTransport Actions

    def set_transport_uri(self, stream_url: str, metadata: AudioMetadata) -> bool:
        """Call SetAVTransportURI to prepare the renderer for streaming."""
        didl_metadata = create_didl_lite_metadata(stream_url, metadata)
        logger.info("Calling SetAVTransportURI with stream URL: %s", stream_url)

        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "SetAVTransportURI",
            {
                "InstanceID": 0,
                "CurrentURI": stream_url,
                "CurrentURIMetaData": didl_metadata,
            },
        )
        if res is not None:
            return True

        # Fallback: Retry with empty CurrentURIMetaData (required by many stereos that reject custom DIDL)
        logger.info("Retrying SetAVTransportURI with empty metadata for %s...", self.device.friendly_name)
        res_fallback = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "SetAVTransportURI",
            {
                "InstanceID": 0,
                "CurrentURI": stream_url,
                "CurrentURIMetaData": "",
            },
        )
        return res_fallback is not None

    def play(self, speed: str = "1", retries: int = 3, retry_delay: float = 0.35) -> bool:
        """Send Play command with automatic retry if renderer is transitioning."""
        self.is_paused_via_stop = False
        for attempt in range(max(1, retries)):
            res = self._send_soap_action(
                self.device.av_transport_control_url,
                AV_TRANSPORT_SERVICE,
                "Play",
                {
                    "InstanceID": 0,
                    "Speed": speed,
                },
            )
            if res is not None:
                return True
            if "701" in self.last_error or "Transition not available" in self.last_error:
                if attempt < retries - 1:
                    logger.info("Renderer is transitioning (701); retrying Play (%d/%d)...", attempt + 1, retries)
                    time.sleep(retry_delay)
                    continue
                return True
            break
        return False

    def pause(self) -> bool:
        """Send Pause command, with automatic fallback to Stop if Pause is unsupported (UPnP 401)."""
        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Pause",
            {
                "InstanceID": 0,
            },
        )
        if res is not None:
            self.is_paused_via_stop = False
            return True

        # Fallback: Many renderers do not support optional UPnP Pause (Error 401: Invalid Action).
        # We gracefully fall back to calling Stop on the renderer.
        if "401" in self.last_error or "Invalid Action" in self.last_error:
            logger.info("Renderer does not support UPnP Pause (Error 401); falling back to Stop")
            stop_res = self.stop()
            self.is_paused_via_stop = True
            return stop_res

        return False

    def stop(self, timeout: Optional[float] = None) -> bool:
        """Send Stop command."""
        self.is_paused_via_stop = False
        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Stop",
            {
                "InstanceID": 0,
            },
            timeout=timeout,
        )
        if res is not None:
            return True
        if "701" in self.last_error or "Transition not available" in self.last_error:
            logger.info("Renderer is already stopped (UPnP 701)")
            return True
        return False

    def seek(self, target_seconds: float) -> bool:
        """Seek to specific position in track (REL_TIME)."""
        target_str = format_seconds_to_upnp_time(target_seconds)
        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Seek",
            {
                "InstanceID": 0,
                "Unit": "REL_TIME",
                "Target": target_str,
            },
        )
        return res is not None

    def get_transport_info(self, timeout: float = 1.5) -> TransportInfo:
        """Get current transport state (PLAYING, PAUSED_PLAYBACK, STOPPED)."""
        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "GetTransportInfo",
            {
                "InstanceID": 0,
            },
            timeout=timeout,
        )
        state = self._extract_response_value(res, "CurrentTransportState") or "STOPPED"
        status = self._extract_response_value(res, "CurrentTransportStatus") or "OK"
        speed = self._extract_response_value(res, "CurrentSpeed") or "1"
        return TransportInfo(state=state, status=status, speed=speed)

    def get_position_info(self, timeout: float = 1.5) -> PositionInfo:
        """Get current playback progress and duration."""
        res = self._send_soap_action(
            self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "GetPositionInfo",
            {
                "InstanceID": 0,
            },
            timeout=timeout,
        )
        rel_time_str = self._extract_response_value(res, "RelTime") or "00:00:00"
        duration_str = self._extract_response_value(res, "TrackDuration") or "00:00:00"
        track_uri = self._extract_response_value(res, "TrackURI") or ""

        rel_time_sec = parse_upnp_time_to_seconds(rel_time_str)
        duration_sec = parse_upnp_time_to_seconds(duration_str)

        # Format as MM:SS if under an hour
        def fmt(s: float) -> str:
            total = int(s)
            h = total // 3600
            m = (total % 3600) // 60
            sec = total % 60
            if h > 0:
                return f"{h:02d}:{m:02d}:{sec:02d}"
            return f"{m:02d}:{sec:02d}"

        return PositionInfo(
            rel_time_seconds=rel_time_sec,
            track_duration_seconds=duration_sec,
            rel_time_formatted=fmt(rel_time_sec),
            track_duration_formatted=fmt(duration_sec),
            track_uri=track_uri,
        )

    # RenderingControl Actions (Volume & Mute)

    def get_volume(self, channel: str = "Master", timeout: float = 1.5) -> Optional[int]:
        """Get current volume (0 - 100)."""
        if not self.device.rendering_control_url:
            return None
        res = self._send_soap_action(
            self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "GetVolume",
            {
                "InstanceID": 0,
                "Channel": channel,
            },
            timeout=timeout,
        )
        val = self._extract_response_value(res, "CurrentVolume")
        if val is not None:
            try:
                return int(val)
            except ValueError:
                pass
        return None

    def set_volume(self, volume: int, channel: str = "Master") -> bool:
        """Set volume (clamped between 0 and 100)."""
        if not self.device.rendering_control_url:
            return False
        clamped_vol = max(0, min(100, volume))
        res = self._send_soap_action(
            self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "SetVolume",
            {
                "InstanceID": 0,
                "Channel": channel,
                "DesiredVolume": clamped_vol,
            },
        )
        return res is not None

    def get_mute(self, channel: str = "Master", timeout: float = 1.5) -> Optional[bool]:
        """Check if stereo is currently muted."""
        if not self.device.rendering_control_url:
            return None
        res = self._send_soap_action(
            self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "GetMute",
            {
                "InstanceID": 0,
                "Channel": channel,
            },
            timeout=timeout,
        )
        val = self._extract_response_value(res, "CurrentMute")
        if val is not None:
            return val in ("1", "true", "True")
        return None

    def set_mute(self, muted: bool, channel: str = "Master") -> bool:
        """Toggle or set mute state."""
        if not self.device.rendering_control_url:
            return False
        res = self._send_soap_action(
            self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "SetMute",
            {
                "InstanceID": 0,
                "Channel": channel,
                "DesiredMute": "1" if muted else "0",
            },
        )
        return res is not None
