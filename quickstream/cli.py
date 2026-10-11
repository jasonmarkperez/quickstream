"""Command-line interface and main entrypoint for Quickstream."""

import argparse
import atexit
import curses
import logging
from pathlib import Path
import signal
import sys
import time
from typing import Optional

from quickstream import __version__
from quickstream.discovery import (
    DLNADevice,
    DeviceManager,
    discover_devices,
    fetch_device_description,
    get_local_ip_for_target,
)
from quickstream.metadata import (
    SUPPORTED_EXTENSIONS,
    AudioMetadata,
    extract_metadata,
    is_supported_audio_file,
)
from quickstream.queue import PlaybackQueue
from quickstream.server import StreamServerManager
from quickstream.transcoder import AudioTranscoder
from quickstream.tui import AppTUI
from quickstream.upnp import UPnPClient

logger = logging.getLogger("quickstream")


def parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(
        prog="quickstream",
        description="Stream audio files over DLNA/UPnP to network stereos with interactive controls.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Audio file(s) or folder(s) to add to queue (.mp3, .flac, .wav, .aiff)",
    )
    parser.add_argument(
        "-d",
        "--device",
        metavar="DEVICE",
        help="Target stereo name or IP address (auto-discovers if omitted)",
    )
    parser.add_argument(
        "-s",
        "--scan",
        action="store_true",
        help="Scan network for DLNA/UPnP stereos and list them",
    )
    parser.add_argument(
        "-i",
        "--info",
        action="store_true",
        help="Print audio metadata of the first specified file and exit",
    )
    parser.add_argument(
        "-t",
        "--transcode",
        action="store_true",
        help="Force transcoding to standard 16-bit PCM WAV via ffmpeg",
    )
    parser.add_argument(
        "-p",
        "--port",
        type=int,
        default=0,
        help="Local HTTP streaming port (default: auto free port)",
    )
    parser.add_argument(
        "--host-ip",
        metavar="IP",
        help="Host IP address for streaming (auto-detected if omitted)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=3.0,
        help="SSDP discovery timeout in seconds (default: 3.0)",
    )
    parser.add_argument(
        "-v",
        "--version",
        action="version",
        version=f"%(prog)s {__version__}",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Enable debug logging to quickstream_debug.log",
    )

    return parser.parse_args(argv)


def setup_logging(debug: bool) -> None:
    """Configure logging to quickstream.log."""
    lvl = logging.DEBUG if debug else logging.INFO
    logging.basicConfig(
        filename="quickstream.log",
        filemode="a",
        level=lvl,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    )


def run_scan(timeout: float) -> int:
    """Quick CLI scan and list DLNA renderers on the network."""
    print(f"Scanning for DLNA / UPnP stereos on local network (timeout {timeout:.1f}s)...")
    devices = discover_devices(timeout=timeout)

    if not devices:
        print("\nNo DLNA / UPnP renderers found.")
        print("Tips:")
        print("  • Ensure your stereo is powered on and connected to the same Wi-Fi/Ethernet.")
        print("  • Check that DLNA / UPnP DMR (Digital Media Renderer) is enabled in your stereo's settings.")
        return 1

    print(f"\nFound {len(devices)} device(s):")
    print("─" * 65)
    for idx, d in enumerate(devices, 1):
        details = []
        if d.manufacturer:
            details.append(d.manufacturer)
        if d.model_name and d.model_name != d.friendly_name:
            details.append(d.model_name)
        detail_str = f" [{', '.join(details)}]" if details else ""
        print(f"  [{idx}] {d.friendly_name}{detail_str}")
        print(f"      IP: {d.host}:{d.port}")
        print(f"      Control: {d.av_transport_control_url}")
    print("─" * 65)
    return 0


def display_metadata_info(meta: AudioMetadata) -> None:
    """Print audio metadata in a clean card."""
    print("\nAudio File Information:")
    print("─" * 50)
    print(f"  File:        {meta.filename}")
    print(f"  Format:      {meta.format_name} ({meta.mime_type})")
    print(f"  Title:       {meta.display_title}")
    if meta.artist:
        print(f"  Artist:      {meta.artist}")
    if meta.album:
        print(f"  Album:       {meta.album}")
    print(f"  Duration:    {meta.duration_formatted} ({meta.duration_seconds:.1f}s)")
    if meta.sample_rate:
        print(f"  Sample Rate: {meta.sample_rate} Hz")
    if meta.bitrate:
        print(f"  Bitrate:     {meta.bitrate // 1000} kbps")
    size_mb = meta.file_size / (1024 * 1024)
    print(f"  File Size:   {size_mb:.2f} MB")
    print("─" * 50)


def main(argv: Optional[list[str]] = None) -> int:
    """Main CLI application entrypoint."""
    args = parse_args(argv)
    setup_logging(args.debug)

    # CLI scan mode
    if args.scan:
        return run_scan(args.timeout)

    # If --info requested with a file
    if args.info:
        if not args.files:
            print("Error: No audio file specified for --info.")
            return 1
        path = Path(args.files[0]).resolve()
        if not path.exists():
            print(f"Error: File not found: {path}")
            return 1
        try:
            meta = extract_metadata(path)
            display_metadata_info(meta)
            return 0
        except Exception as e:
            print(f"Error reading audio file: {e}")
            return 1

    # Initialize components
    queue = PlaybackQueue()
    device_manager = DeviceManager()
    transcoder = AudioTranscoder()

    # Pre-populate playlist if files/folders provided on command line
    for item in args.files:
        p = Path(item).resolve()
        if p.is_dir():
            queue.add_directory(p)
        elif p.is_file() and is_supported_audio_file(p):
            queue.add_file(p)

    initial_device: Optional[DLNADevice] = None

    # If target device specified via CLI
    if args.device:
        target = args.device.strip()
        # Direct URL or IP check
        dev = device_manager.add_manual_device(target)
        if dev:
            initial_device = dev
        else:
            # Quick discovery to find matching device
            found = discover_devices(timeout=args.timeout)
            for d in found:
                if target.lower() in d.friendly_name.lower() or target.lower() in d.host.lower():
                    initial_device = d
                    device_manager.devices.append(d)
                    break

    # Start HTTP streaming server in background
    if not initial_device:
        for d in device_manager.devices:
            if d.udn == "local-audio":
                initial_device = d
                break

    if initial_device:
        host_ip = args.host_ip or get_local_ip_for_target(initial_device.host, initial_device.port)
    else:
        host_ip = args.host_ip or get_local_ip_for_target("239.255.255.250", 1900)

    server_manager = StreamServerManager(
        metadata=queue.get_current_track(),
        host_ip=host_ip,
        preferred_port=args.port,
    )
    server_manager.start()

    # Launch full in-place TUI
    app = AppTUI(
        device_manager=device_manager,
        queue=queue,
        server_manager=server_manager,
        transcoder=transcoder,
        initial_device=initial_device,
    )

    cleaned_up = False

    def cleanup_all():
        nonlocal cleaned_up
        if cleaned_up:
            return
        cleaned_up = True
        logger.info("Executing shutdown cleanup: stopping stereo playback, server sockets, and temp files")
        try:
            app.cleanup()
        except Exception:
            pass
        try:
            server_manager.stop()
        except Exception:
            pass
        try:
            transcoder.cleanup()
        except Exception:
            pass

    atexit.register(cleanup_all)

    def _signal_handler(signum, frame):
        cleanup_all()
        try:
            curses.endwin()
        except Exception:
            pass
        sys.exit(0)

    # Prevent SIGPIPE from terminating the process if stereo abruptly disconnects
    if hasattr(signal, "SIGPIPE"):
        try:
            signal.signal(signal.SIGPIPE, signal.SIG_IGN)
        except (ValueError, OSError):
            pass

    for sig in (signal.SIGTERM, signal.SIGHUP):
        try:
            signal.signal(sig, _signal_handler)
        except (ValueError, OSError):
            pass

    try:
        app.run()
    except KeyboardInterrupt:
        pass
    except Exception as e:
        logger.error("Application crashed: %s", e, exc_info=True)
        print(f"App error: {e}", file=sys.stderr)
    finally:
        cleanup_all()
        try:
            atexit.unregister(cleanup_all)
        except Exception:
            pass

    return 0


if __name__ == "__main__":
    sys.exit(main())
