"""SSDP UPnP Discovery and Device Description parsing for DLNA MediaRenderers."""

from dataclasses import dataclass
import socket
from typing import Dict, List, Optional
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


@dataclass
class DLNADevice:
    """Represents a discovered DLNA MediaRenderer on the local network."""

    friendly_name: str
    location: str
    host: str
    port: int
    manufacturer: str = ""
    model_name: str = ""
    av_transport_control_url: str = ""
    rendering_control_url: str = ""
    udn: str = ""

    def __str__(self) -> str:
        details = []
        if self.manufacturer:
            details.append(self.manufacturer)
        if self.model_name and self.model_name != self.friendly_name:
            details.append(self.model_name)
        detail_str = f" ({', '.join(details)})" if details else ""
        return f"{self.friendly_name}{detail_str} @ {self.host}:{self.port}"


def get_local_ip_for_target(target_host: str, target_port: int = 80) -> str:
    """Determine the local IPv4 address routable to the target host."""
    # 1. Direct UDP connect to target host
    try:
        port = int(target_port) if target_port and int(target_port) > 0 else 80
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(0.5)
        s.connect((target_host, port))
        ip = s.getsockname()[0]
        s.close()
        if ip and not ip.startswith("127."):
            return ip
    except Exception:
        pass

    # 2. Probe common gateway / internet destinations
    for probe in ["192.168.1.1", "192.168.0.1", "10.0.0.1", "8.8.8.8", "1.1.1.1"]:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.settimeout(0.5)
            s.connect((probe, 80))
            ip = s.getsockname()[0]
            s.close()
            if ip and not ip.startswith("127."):
                return ip
        except Exception:
            pass

    # 3. Parse system network interfaces (macOS/Linux)
    try:
        import subprocess
        out = subprocess.check_output(["ifconfig"], text=True, stderr=subprocess.DEVNULL)
        for line in out.splitlines():
            line = line.strip()
            if line.startswith("inet ") and not line.startswith("inet 127."):
                parts = line.split()
                if len(parts) >= 2 and parts[1]:
                    return parts[1]
    except Exception:
        pass

    return "127.0.0.1"


def _strip_ns(tag: str) -> str:
    """Strip XML namespace prefix from tag name."""
    if "}" in tag:
        return tag.split("}", 1)[1]
    return tag


def parse_device_description(xml_data: str, location_url: str) -> Optional[DLNADevice]:
    """Parse UPnP Device Description XML and extract AVTransport & RenderingControl services."""
    try:
        root = ET.fromstring(xml_data)
    except ET.ParseError:
        return None

    parsed_location = urllib.parse.urlparse(location_url)
    host = parsed_location.hostname or ""
    port = parsed_location.port or 80

    friendly_name = ""
    manufacturer = ""
    model_name = ""
    udn = ""
    av_transport_control = ""
    rendering_control = ""

    # Traverse all elements
    for elem in root.iter():
        tag = _strip_ns(elem.tag)
        if tag == "friendlyName" and not friendly_name:
            friendly_name = (elem.text or "").strip()
        elif tag == "manufacturer" and not manufacturer:
            manufacturer = (elem.text or "").strip()
        elif tag == "modelName" and not model_name:
            model_name = (elem.text or "").strip()
        elif tag == "UDN" and not udn:
            udn = (elem.text or "").strip()
        elif tag == "service":
            # Extract service details
            stype = ""
            curl = ""
            for child in elem:
                ctag = _strip_ns(child.tag)
                if ctag == "serviceType":
                    stype = (child.text or "").strip()
                elif ctag == "controlURL":
                    curl = (child.text or "").strip()

            if "service:AVTransport:" in stype:
                av_transport_control = urllib.parse.urljoin(location_url, curl)
            elif "service:RenderingControl:" in stype:
                rendering_control = urllib.parse.urljoin(location_url, curl)

    # Must provide AVTransport service to be a usable MediaRenderer
    if not av_transport_control:
        return None

    if not friendly_name:
        friendly_name = f"MediaRenderer ({host})"

    return DLNADevice(
        friendly_name=friendly_name,
        location=location_url,
        host=host,
        port=port,
        manufacturer=manufacturer,
        model_name=model_name,
        av_transport_control_url=av_transport_control,
        rendering_control_url=rendering_control,
        udn=udn,
    )


def fetch_device_description(location_url: str, timeout: float = 3.0) -> Optional[DLNADevice]:
    """Download and parse UPnP XML device description from a given URL."""
    try:
        req = urllib.request.Request(
            location_url,
            headers={"User-Agent": "Quickstream/0.1.0 UPnP/1.1 DLNADOC/1.50"},
        )
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(req, timeout=timeout) as resp:
            xml_text = resp.read().decode("utf-8", errors="replace")
            return parse_device_description(xml_text, location_url)
    except Exception:
        return None


def discover_devices(timeout: float = 3.5, search_target: Optional[str] = None) -> List[DLNADevice]:
    """Discover DLNA MediaRenderers on the local network via SSDP multicast."""
    search_targets = [
        search_target
    ] if search_target else [
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        "urn:schemas-upnp-org:service:AVTransport:1",
    ]

    multicast_group = "239.255.255.250"
    multicast_port = 1900

    discovered_locations: set[str] = set()
    devices: List[DLNADevice] = []

    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        sock.settimeout(0.5)
        try:
            sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
        except Exception:
            pass

        for st in search_targets:
            message = (
                f"M-SEARCH * HTTP/1.1\r\n"
                f"HOST: {multicast_group}:{multicast_port}\r\n"
                f'MAN: "ssdp:discover"\r\n'
                f"MX: 2\r\n"
                f"ST: {st}\r\n"
                f"\r\n"
            )
            try:
                sock.sendto(message.encode("utf-8"), (multicast_group, multicast_port))
            except Exception:
                pass

        import time
        start_time = time.time()

        while time.time() - start_time < timeout:
            try:
                data, _ = sock.recvfrom(4096)
                response = data.decode("utf-8", errors="replace")

                # Parse HTTP headers in response
                lines = response.splitlines()
                location = None
                for line in lines:
                    if ":" in line:
                        header_name, header_val = line.split(":", 1)
                        if header_name.strip().upper() == "LOCATION":
                            location = header_val.strip()
                            break

                if location and location not in discovered_locations:
                    discovered_locations.add(location)
                    device = fetch_device_description(location, timeout=2.0)
                    if device:
                        devices.append(device)
            except socket.timeout:
                continue
            except Exception:
                break

    except Exception:
        pass
    finally:
        try:
            sock.close()
        except Exception:
            pass

    # Deduplicate by AVTransport control URL or UDN
    seen_urls: set[str] = set()
    unique_devices: List[DLNADevice] = []
    for d in devices:
        key = d.udn or d.av_transport_control_url
        if key not in seen_urls:
            seen_urls.add(key)
            unique_devices.append(d)

    return unique_devices


class DeviceManager:
    """Manages discovered DLNA devices and non-blocking background network scans."""

    def __init__(self):
        self.devices: List[DLNADevice] = [
            DLNADevice(
                friendly_name="Local Audio",
                location="localhost",
                host="localhost",
                port=0,
                manufacturer="Quickstream",
                udn="local-audio"
            )
        ]
        self.active_device: Optional[DLNADevice] = None
        self.is_scanning: bool = False
        self._scan_thread = None

    def start_scan(self, timeout: float = 3.0, on_complete=None) -> None:
        """Start SSDP discovery scan in a background thread."""
        if self.is_scanning:
            return

        import threading

        def _worker():
            self.is_scanning = True
            try:
                try:
                    found = discover_devices(timeout=timeout)
                except Exception:
                    found = []
                # Merge found devices preserving active
                existing_keys = {d.udn or d.av_transport_control_url: d for d in self.devices}
                for d in found:
                    k = d.udn or d.av_transport_control_url
                    existing_keys[k] = d
                    
                # Always ensure Local Audio device exists
                if "local-audio" not in existing_keys:
                    local_dev = DLNADevice(
                        friendly_name="Local Audio",
                        location="localhost",
                        host="localhost",
                        port=0,
                        manufacturer="Quickstream",
                        udn="local-audio"
                    )
                    existing_keys["local-audio"] = local_dev
                    
                self.devices = list(existing_keys.values())

                # Auto-select if no active device
                if not self.active_device:
                    ext_devices = [d for d in self.devices if d.udn != "local-audio"]
                    if len(ext_devices) == 1:
                        self.active_device = ext_devices[0]
                    elif len(self.devices) == 1:
                        self.active_device = self.devices[0]

                if on_complete:
                    on_complete(self.devices)
            except Exception:
                pass
            finally:
                self.is_scanning = False

        self._scan_thread = threading.Thread(target=_worker, daemon=True, name="DLNAScanner")
        self._scan_thread.start()

    def set_active_device(self, device: DLNADevice) -> None:
        """Select active device for streaming."""
        self.active_device = device

    def add_manual_device(self, ip_or_url: str) -> Optional[DLNADevice]:
        """Try fetching device description directly by IP or URL."""
        url = ip_or_url.strip()
        if not url.startswith("http://") and not url.startswith("https://"):
            # Try default UPnP ports
            candidates = [
                f"http://{url}:8080/description.xml",
                f"http://{url}:49152/description.xml",
                f"http://{url}:49153/description.xml",
                f"http://{url}:80/description.xml",
            ]
        else:
            candidates = [url]

        for cand in candidates:
            dev = fetch_device_description(cand, timeout=1.5)
            if dev:
                if not any((d.udn or d.av_transport_control_url) == (dev.udn or dev.av_transport_control_url) for d in self.devices):
                    self.devices.append(dev)
                return dev
        return None

