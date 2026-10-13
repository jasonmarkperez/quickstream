"""All-in-one curses Terminal User Interface for Quickstream."""

import curses
import logging
from pathlib import Path
import time
from typing import Optional, Any

from quickstream.browser import FileBrowser
from quickstream.discovery import DLNADevice, DeviceManager, get_local_ip_for_target
from quickstream.metadata import AudioMetadata
from quickstream.queue import PlaybackQueue
from quickstream.server import StreamServerManager
from quickstream.transcoder import AudioTranscoder
from quickstream.upnp import PositionInfo, TransportInfo, UPnPClient, format_seconds_to_upnp_time
from quickstream.local_player import LocalPlayerClient
from quickstream.settings import SettingsManager

logger = logging.getLogger("quickstream.tui")


class AppTUI:
    """All-in-one curses terminal application for UPnP audio streaming."""

    def __init__(
        self,
        device_manager: DeviceManager,
        queue: PlaybackQueue,
        server_manager: StreamServerManager,
        transcoder: AudioTranscoder,
        initial_device: Optional[DLNADevice] = None,
    ):
        self.device_manager = device_manager
        self.queue = queue
        self.server_manager = server_manager
        self.transcoder = transcoder

        # Settings manager & active configuration
        self.settings_manager = SettingsManager()
        self.settings = self.settings_manager.settings
        self.settings_cursor = 0

        self.client = None
        if initial_device:
            self.device_manager.set_active_device(initial_device)
            if initial_device.udn == "local-audio":
                self.client = LocalPlayerClient(initial_device)
            else:
                self.client = UPnPClient(initial_device)

        # Tabs: 0 = Queue, 1 = Devices, 2 = Settings
        self.active_tab = 0
        self.show_help_modal = False
        self.file_browser: Optional[FileBrowser] = None

        # Playback state
        self.transport_info = TransportInfo()
        self.position_info = PositionInfo()
        self.current_volume: int = 50
        self.is_muted: bool = False
        self.was_playing: bool = False

        # Playback timing estimation & transcoding
        self.playback_start_time: float = 0.0
        self.playback_start_position: float = 0.0
        self.playback_stream_offset: float = 0.0
        self.paused_position: float = 0.0
        self.user_stopped: bool = False
        self.force_transcode: bool = self.settings.force_transcode
        self.stereo_supports_upnp_seek: bool = True
        
        # Accumulated seeking state
        self.pending_seek_delta: float = 0.0
        self.last_seek_key_time: float = 0.0

        # Selection cursors for tabs
        self.queue_cursor = 0
        self.queue_offset = 0
        self.device_cursor = 0
        self.device_offset = 0

        # UI state
        self.status_message: str = "Welcome to Quickstream"
        self.status_timestamp: float = time.time()
        self.running: bool = True
        self.file_browser = FileBrowser(metadata_view=self.settings.metadata_view)
        self.queue_focus = "queue"  # or "browser"

    def set_status(self, message: str) -> None:
        """Set transient message."""
        self.status_message = message
        self.status_timestamp = time.time()

    def cleanup(self) -> None:
        """Safely release playback on connected stereo and restore resources."""
        if self.client:
            try:
                self.client.stop(timeout=1.5)
            except Exception as e:
                logger.debug("Stereo cleanup error: %s", e)

    def run(self) -> None:
        """Run curses application loop with guaranteed cleanup."""
        try:
            curses.wrapper(self._main_loop)
        finally:
            self.cleanup()

    def _init_colors(self) -> None:
        curses.start_color()
        curses.use_default_colors()
        try:
            curses.init_pair(1, curses.COLOR_CYAN, -1)     # Titles / Headers
            curses.init_pair(2, curses.COLOR_GREEN, -1)    # Playing / Success
            curses.init_pair(3, curses.COLOR_YELLOW, -1)   # Warning / Paused
            curses.init_pair(4, curses.COLOR_RED, -1)      # Error / Stopped
            curses.init_pair(5, curses.COLOR_MAGENTA, -1)  # Accents / Format
            curses.init_pair(6, curses.COLOR_WHITE, -1)    # Active selection
        except Exception:
            pass

    def play_track(self, index: int) -> bool:
        """Start streaming a track from the queue to active stereo."""
        track = self.queue.set_current_index(index)
        if not track:
            self.set_status("Invalid track selection")
            return False

        # Keep queue cursor in sync with current playing track
        self.queue_cursor = index

        if not self.device_manager.active_device:
            self.set_status("No stereo connected. Switch to [3] Devices tab.")
            self.active_tab = 2
            return False

        device = self.device_manager.active_device
        if not self.client or self.client.device != device:
            if self.client:
                try:
                    self.client.stop()
                except Exception:
                    pass
            if device.udn == "local-audio":
                self.client = LocalPlayerClient(device)
            else:
                self.client = UPnPClient(device)

        # Check local IP routable to stereo
        host_ip = get_local_ip_for_target(device.host, device.port)
        self.server_manager.set_host_ip(host_ip)

        stream_meta = track
        # Transcode if forced or if format is AIFF and ffmpeg available
        needs_transcode = self.force_transcode or (track.format_name == "AIFF")
        if needs_transcode and self.transcoder.is_available():
            trans_meta = self.transcoder.transcode_to_wav(track)
            if trans_meta:
                stream_meta = trans_meta
                logger.info("Using transcoded PCM WAV for %s", track.filename)

        stream_url = self.server_manager.get_stream_url(stream_meta)

        self.set_status(f"Loading track: {track.display_title}...")

        # 1. Close existing client sockets on server to abort previous stream cleanly
        srv = getattr(self.server_manager, "server", None)
        if srv and hasattr(srv, "close_all_client_sockets"):
            srv.close_all_client_sockets()

        # 2. Tell stereo to stop previous track with a safe timeout
        try:
            self.client.stop(timeout=1.5)
            time.sleep(0.2)
        except Exception:
            pass

        # 3. Set new transport URI with retry
        uri_ok = False
        for uri_attempt in range(2):
            if self.client.set_transport_uri(stream_url, stream_meta):
                uri_ok = True
                break
            time.sleep(0.25)

        if not uri_ok:
            err = self.client.last_error or f"Rejected by {device.friendly_name}"
            self.set_status(f"Error loading track: {err}")
            return False

        # 4. Send Play command with retry
        if not self.client.play():
            err = self.client.last_error or f"Play command rejected by {device.friendly_name}"
            self.set_status(f"Error starting playback: {err}")
            return False

        self.transport_info.state = "PLAYING"
        self.position_info.track_duration_seconds = track.duration_seconds
        self.position_info.track_duration_formatted = track.duration_formatted
        self.position_info.rel_time_seconds = 0
        self.position_info.rel_time_formatted = "00:00"
        self.playback_start_time = time.time()
        self.playback_start_position = 0.0
        self.playback_stream_offset = 0.0
        self.paused_position = 0.0
        self.user_stopped = False
        self.was_playing = True
        self.set_status(f"Playing: {track.display_title}")

        return True

    def _safe_addstr(self, stdscr: curses.window, y: int, x: int, text: str, attr: int = 0) -> None:
        """Safely write string within screen boundaries without raising curses.error."""
        try:
            max_y, max_x = stdscr.getmaxyx()
            if y < 0 or y >= max_y or x < 0 or x >= max_x:
                return
            max_len = max_x - x
            if y == max_y - 1:
                max_len -= 1  # Avoid bottom-right corner character error in curses
            if max_len <= 0:
                return
            clipped = text[:max_len]
            stdscr.addstr(y, x, clipped, attr)
        except curses.error:
            pass

    def _draw_progress_bar(self, width: int, current: float, total: float) -> str:
        if width < 10:
            width = 10
        ratio = 0.0
        if total > 0:
            ratio = max(0.0, min(1.0, current / total))
        filled_len = int(ratio * width)
        empty_len = width - filled_len
        if filled_len > 0:
            return "━" * (filled_len - 1) + "●" + "─" * empty_len
        return "●" + "─" * (width - 1)

    def _draw_volume_bar(self, width: int, volume: int) -> str:
        if width < 5:
            width = 5
        ratio = max(0.0, min(1.0, volume / 100.0))
        filled_len = int(ratio * width)
        empty_len = width - filled_len
        return "█" * filled_len + "░" * empty_len

    def _render_tabs_header(self, stdscr: curses.window, max_x: int) -> None:
        """Render top navigation bar."""
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)

        self._safe_addstr(stdscr, 1, 2, "QUICKSTREAM", cyan | bold)

        tab_defs = [
            (0, f"[1] Queue ({len(self.queue)})"),
            (1, f"[2] Devices ({len(self.device_manager.devices)})" + (" [SCANNING...]" if self.device_manager.is_scanning else "")),
            (2, "[3] Settings"),
        ]

        col = 16
        for tab_id, label in tab_defs:
            is_active = self.active_tab == tab_id
            if is_active:
                self._safe_addstr(stdscr, 1, col, f" {label} ", bold | curses.A_REVERSE)
            else:
                self._safe_addstr(stdscr, 1, col, f" {label} ", bold)
            col += len(label) + 4

        line = "━" * min(max_x - 4, 76)
        self._safe_addstr(stdscr, 2, 2, line, curses.A_DIM)

    def _render_queue(self, stdscr: curses.window, max_y: int, max_x: int) -> None:
        """Render Tab 1: Queue & Media Browser."""
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)
        green = curses.color_pair(2)
        yellow = curses.color_pair(3)

        mid_x = max_x // 2
        
        # Draw Separator
        for y in range(2, max_y - 2):
            self._safe_addstr(stdscr, y, mid_x, "│", curses.A_DIM)

        # ---------------------------
        # LEFT PANE: File Browser
        # ---------------------------
        if not self.file_browser:
            self.file_browser = FileBrowser()
            
        self.file_browser.render(
            stdscr=stdscr,
            start_y=2,
            start_x=0,
            height=max_y - 4,
            width=mid_x - 1,
            is_focused=(self.queue_focus == "browser")
        )

        # ---------------------------
        # RIGHT PANE: Queue
        # ---------------------------
        q_start_x = mid_x + 2
        q_width = max_x - q_start_x - 1
        
        title = " QUEUE " if self.queue_focus == "queue" else " queue "
        title_attr = (cyan | bold) if self.queue_focus == "queue" else (cyan | curses.A_DIM)
        
        self._safe_addstr(stdscr, 2, q_start_x, title, title_attr)
        track_count = f"{len(self.queue)} tracks"
        self._safe_addstr(stdscr, 2, q_start_x + 18, track_count, curses.A_DIM)
        self._safe_addstr(stdscr, 3, q_start_x, "─" * min(q_width, 76), curses.A_DIM)

        list_height = max(1, max_y - 7)
        
        # Adjust scroll offset
        if self.queue_cursor < self.queue_offset:
            self.queue_offset = self.queue_cursor
        elif self.queue_cursor >= self.queue_offset + list_height:
            self.queue_offset = self.queue_cursor - list_height + 1
            
        visible_tracks = self.queue.tracks[self.queue_offset : self.queue_offset + list_height]

        if not self.queue.tracks:
            self._safe_addstr(stdscr, 5, q_start_x, "(Queue is empty)", yellow)
        else:
            for idx, track in enumerate(visible_tracks):
                actual_idx = self.queue_offset + idx
                row = 4 + idx
                if row >= max_y - 3:
                    break
                    
                is_cursor = (actual_idx == self.queue_cursor) and (self.queue_focus == "queue")
                is_playing = actual_idx == self.queue.current_index

                play_badge = "▶" if is_playing else " "
                cursor_badge = ">" if is_cursor else " "

                duration = track.duration_formatted
                fmt = track.format_name
                if self.settings.metadata_view:
                    t_title = track.display_title
                    artist = f" - {track.artist}" if track.artist else ""
                    item_line = f" {cursor_badge} [{play_badge}] {actual_idx + 1:2d}. {t_title}{artist} ({duration})"
                else:
                    item_line = f" {cursor_badge} [{play_badge}] {actual_idx + 1:2d}. {track.filename} ({duration})"

                if is_cursor:
                    self._safe_addstr(stdscr, row, q_start_x, item_line.ljust(q_width), bold | curses.A_REVERSE)
                elif is_playing:
                    self._safe_addstr(stdscr, row, q_start_x, item_line[:q_width], green | bold)
                else:
                    attr = curses.A_NORMAL if self.queue_focus == "queue" else curses.A_DIM
                    self._safe_addstr(stdscr, row, q_start_x, item_line[:q_width], attr)

    def _render_devices(self, stdscr: curses.window, max_y: int, max_x: int) -> None:
        """Render Tab 2: Devices & Discovery."""
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)
        green = curses.color_pair(2)
        yellow = curses.color_pair(3)

        self._safe_addstr(stdscr, 3, 2, "DLNA / UPnP MediaRenderers on Local Network:", bold | cyan)

        devices = self.device_manager.devices
        list_height = max(1, max_y - 9)
        
        # Ensure cursor is visible
        if self.device_cursor < self.device_offset:
            self.device_offset = self.device_cursor
        elif self.device_cursor >= self.device_offset + list_height:
            self.device_offset = self.device_cursor - list_height + 1
            
        visible_devices = devices[self.device_offset : self.device_offset + list_height]

        if not devices:
            if self.device_manager.is_scanning:
                self._safe_addstr(stdscr, 5, 4, "Scanning network for UPnP stereos... please wait.", cyan | bold)
            else:
                self._safe_addstr(stdscr, 5, 4, "No DLNA devices discovered yet.", yellow)
                self._safe_addstr(stdscr, 7, 4, "• Press [s] to rescan the local Wi-Fi/LAN.", bold)
                self._safe_addstr(stdscr, 8, 4, "• Make sure your stereo is powered on and connected to the same network.", curses.A_DIM)
        else:
            for idx, dev in enumerate(visible_devices):
                actual_idx = self.device_offset + idx
                row = 5 + idx
                is_cursor = actual_idx == self.device_cursor
                is_active = (
                    self.device_manager.active_device
                    and (dev.udn or dev.av_transport_control_url)
                    == (self.device_manager.active_device.udn or self.device_manager.active_device.av_transport_control_url)
                )

                active_badge = "✔ Connected" if is_active else "           "
                cursor_badge = ">" if is_cursor else " "

                dev_line = f" {cursor_badge} [{active_badge}] {dev.friendly_name} @ {dev.host}:{dev.port}"
                if dev.manufacturer:
                    dev_line += f" ({dev.manufacturer})"

                if is_cursor:
                    self._safe_addstr(stdscr, row, 2, dev_line.ljust(min(max_x - 4, 74)), bold | curses.A_REVERSE)
                elif is_active:
                    self._safe_addstr(stdscr, row, 2, dev_line, green | bold)
                else:
                    self._safe_addstr(stdscr, row, 2, dev_line)

    def _render_settings(self, stdscr: curses.window, max_y: int, max_x: int) -> None:
        """Render Tab 3: Application Settings."""
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)
        yellow = curses.color_pair(3)

        self._safe_addstr(stdscr, 3, 2, "Application Settings & Preferences:", bold | cyan)
        self._safe_addstr(stdscr, 4, 2, "─" * min(max_x - 4, 76), curses.A_DIM)

        defs = self.settings_manager.definitions
        start_row = 5
        for idx, defn in enumerate(defs):
            row = start_row + idx
            if row >= max_y - 6:
                break
            is_cursor = (idx == self.settings_cursor)
            cursor_badge = ">" if is_cursor else " "
            val_str = self.settings_manager.get_display_value(idx)

            line_str = f" {cursor_badge} [{defn.label:<24}] : < {val_str} >"
            if is_cursor:
                self._safe_addstr(stdscr, row, 2, line_str.ljust(min(max_x - 4, 76)), bold | curses.A_REVERSE)
            else:
                self._safe_addstr(stdscr, row, 2, line_str[: max_x - 4], curses.A_NORMAL)

        # Description box
        desc_row = start_row + len(defs) + 1
        if desc_row < max_y - 4:
            self._safe_addstr(stdscr, desc_row, 2, "─" * min(max_x - 4, 76), curses.A_DIM)
            if 0 <= self.settings_cursor < len(defs):
                curr_defn = defs[self.settings_cursor]
                self._safe_addstr(stdscr, desc_row + 1, 2, f"Setting: {curr_defn.label}", bold | yellow)
                self._safe_addstr(stdscr, desc_row + 2, 2, curr_defn.description[: max_x - 4], curses.A_DIM)

        # Navigation hint
        hint_row = max_y - 2
        hints = "[↑/↓] Select   [←/→] / [Enter] Cycle Value   [r] Reset Defaults"
        self._safe_addstr(stdscr, hint_row, 2, hints[: max_x - 4], curses.A_DIM)

    def _render_persistent_playback(self, stdscr: curses.window, start_y: int, max_x: int) -> None:
        """Render persistent Now Playing bar at the bottom."""
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)
        green = curses.color_pair(2)
        dim = curses.A_DIM
        
        self._safe_addstr(stdscr, start_y, 0, "─" * max_x, dim)
        
        track = self.queue.get_current_track()
        if not track:
            self._safe_addstr(stdscr, start_y + 1, 2, "No track playing.", dim)
            self._safe_addstr(stdscr, start_y + 3, 0, "─" * max_x, dim)
            self._safe_addstr(stdscr, start_y + 4, 2, "Press [?] for hotkeys | [q] Quit", dim)
            return

        state = self.transport_info.state.upper()
        if "PLAY" in state:
            state_badge = "▶"
        elif "TRANS" in state:
            state_badge = "⟳"
        elif "PAUSE" in state:
            state_badge = "⏸"
        else:
            state_badge = "⏹"

        title = track.display_title
        artist = f" - {track.artist}" if track.artist else ""
        fmt = track.format_name
        
        line1 = f"{state_badge} {title}{artist}  [{fmt}]"
        self._safe_addstr(stdscr, start_y + 1, 2, line1[:max_x - 15], cyan | bold)
        
        vol_str = f"Vol: {self.current_volume}%"
        if self.is_muted:
            vol_str = "Vol: MUTED"
        self._safe_addstr(stdscr, start_y + 1, max_x - len(vol_str) - 2, vol_str, green)

        cur_sec = self.position_info.rel_time_seconds
        total_sec = self.position_info.track_duration_seconds if self.position_info.track_duration_seconds > 0 else track.duration_seconds
        time_cur = self.position_info.rel_time_formatted
        time_tot = self.position_info.track_duration_formatted if self.position_info.track_duration_seconds > 0 else track.duration_formatted
        
        bar_width = min(max_x - 30, 60)
        p_bar = self._draw_progress_bar(bar_width, cur_sec, total_sec)
        
        line2 = f"  [{p_bar}] {time_cur} / {time_tot}"
        self._safe_addstr(stdscr, start_y + 2, 0, line2)

        self._safe_addstr(stdscr, start_y + 3, 0, "─" * max_x, dim)
        self._safe_addstr(stdscr, start_y + 4, 2, "Press [?] for hotkeys | [q] Quit", dim)

    def _render_help_modal(self, stdscr: curses.window, max_y: int, max_x: int) -> None:
        """Render centered help popup."""
        help_text = [
            "          QUICKSTREAM HELP          ",
            "────────────────────────────────────",
            " Playback:                          ",
            "   [Space] Play/Pause               ",
            "   [s] Stop                         ",
            "   [←/→] Prev/Next Track            ",
            "   [Shift+←/→] Seek relative        ",
            "   [+/-] Volume Up/Down             ",
            "   [m] Mute Toggle                  ",
            "   [t] Toggle Transcoding           ",
            "                                    ",
            " Browser & Queue:                   ",
            "   [PgDn/PgUp] Page Up / Down List  ",
            "   [b] Bookmark / Unbookmark Folder ",
            "   [d/x] Remove Track or Bookmark   ",
            "   [a] Add All in Folder to Queue   ",
            "   [c] Clear Playback Queue         ",
            "   [v] Toggle Metadata/Filename View",
            "                                    ",
            " Navigation:                        ",
            "   [Tab] Switch Main Tabs (1/2/3)   ",
            "   [1/2/3] Jump to Queue/Dev/Setting",
            "   [h/l] Switch Panes in Queue      ",
            "   [?] Close Help                   ",
            "   [q] Quit                         ",
            "                                    ",
            "        Press [Esc] to Close        ",
        ]
        h = len(help_text) + 2
        w = len(help_text[0]) + 4
        start_y = max(0, (max_y - h) // 2)
        start_x = max(0, (max_x - w) // 2)
        
        bold = curses.A_BOLD
        cyan = curses.color_pair(1)
        
        for i in range(h):
            self._safe_addstr(stdscr, start_y + i, start_x, " " * w, curses.A_REVERSE)
            
        for idx, line in enumerate(help_text):
            attr = (cyan | bold | curses.A_REVERSE) if idx == 0 else curses.A_REVERSE
            self._safe_addstr(stdscr, start_y + 1 + idx, start_x + 2, line, attr)

    def _render(self, stdscr: curses.window) -> None:
        """Draw entire UI."""
        stdscr.erase()
        max_y, max_x = stdscr.getmaxyx()
        yellow = curses.color_pair(3)

        self._render_tabs_header(stdscr, max_x)

        view_max_y = max_y - 5
        
        if self.active_tab == 0:
            self._render_queue(stdscr, view_max_y, max_x)
        elif self.active_tab == 1:
            self._render_devices(stdscr, view_max_y, max_x)
        elif self.active_tab == 2:
            self._render_settings(stdscr, view_max_y, max_x)

        self._render_persistent_playback(stdscr, view_max_y, max_x)

        if time.time() - self.status_timestamp < 4.5:
            status_row = view_max_y - 1
            if status_row > 2:
                self._safe_addstr(stdscr, status_row, 2, f"Info: {self.status_message}", yellow)
                
        if getattr(self, "show_help_modal", False):
            self._render_help_modal(stdscr, max_y, max_x)

        stdscr.refresh()
    def _main_loop(self, stdscr: curses.window) -> None:
        curses.curs_set(0)
        stdscr.nodelay(True)
        stdscr.keypad(True)
        self._init_colors()

        # Start initial background scan for stereos if only Local Audio is present
        if len(self.device_manager.devices) <= 1:
            self.device_manager.start_scan(
                timeout=3.0,
                on_complete=lambda devs: self.set_status(f"Scan complete: found {len(devs)} stereos"),
            )

        last_poll_time = 0.0
        last_vol_poll_time = 0.0

        while self.running:
            now = time.time()

            # Poll device if connected and active
            if self.client and (now - last_poll_time >= 0.5):
                last_poll_time = now
                try:
                    t_info = self.client.get_transport_info(timeout=1.2)
                    if t_info:
                        prev_state = self.transport_info.state
                        if getattr(self.client, "is_paused_via_stop", False):
                            self.transport_info.state = "PAUSED_PLAYBACK"
                        elif self.user_stopped:
                            self.transport_info.state = "STOPPED"
                        else:
                            self.transport_info = t_info

                        # Auto-advance logic: if track transitions from PLAYING to STOPPED
                        curr_meta = self.queue.get_current_track()
                        near_end = (
                            curr_meta
                            and curr_meta.duration_seconds > 0
                            and self.position_info.rel_time_seconds >= max(0.0, curr_meta.duration_seconds - 5.0)
                        )
                        if (
                            not self.user_stopped
                            and not getattr(self.client, "is_paused_via_stop", False)
                            and prev_state.upper() == "PLAYING"
                            and "STOP" in t_info.state.upper()
                            and near_end
                        ):
                            mode = getattr(self.settings, "queue_mode", "loop")
                            if mode == "repeat_one":
                                self.set_status("Repeating current track...")
                                self.play_track(self.queue.current_index)
                            elif self.queue.has_next():
                                self.set_status("Track finished. Advancing to next track...")
                                self.play_track(self.queue.current_index + 1)
                            elif mode == "loop" and len(self.queue) > 0:
                                self.set_status("Queue finished. Looping to start...")
                                self.play_track(0)
                            else:
                                self.set_status("Queue finished. Playback stopped.")

                    if not getattr(self.client, "is_paused_via_stop", False) and not self.user_stopped:
                        p_info = self.client.get_position_info(timeout=1.2)
                        curr_meta = self.queue.get_current_track()
                        if p_info and self.pending_seek_delta == 0.0:
                            if p_info.track_duration_seconds == 0 and curr_meta and curr_meta.duration_seconds > 0:
                                p_info.track_duration_seconds = curr_meta.duration_seconds
                                p_info.track_duration_formatted = curr_meta.duration_formatted

                            # If stereo reports a positive advancing position, use it
                            offset = getattr(self, "playback_stream_offset", 0.0)
                            if p_info.rel_time_seconds > 0:
                                if offset > 0 and p_info.rel_time_seconds < offset:
                                    actual_pos = offset + p_info.rel_time_seconds
                                else:
                                    actual_pos = p_info.rel_time_seconds
                                total = (
                                    curr_meta.duration_seconds
                                    if curr_meta and curr_meta.duration_seconds > 0
                                    else p_info.track_duration_seconds
                                )
                                if total > 0:
                                    actual_pos = min(total, actual_pos)
                                self.position_info.rel_time_seconds = actual_pos
                                self.position_info.track_duration_seconds = total
                                self.position_info.track_duration_formatted = (
                                    curr_meta.duration_formatted if curr_meta else p_info.track_duration_formatted
                                )
                                self.playback_start_position = actual_pos
                                self.playback_start_time = now
                                sec = int(actual_pos)
                                m = sec // 60
                                s = sec % 60
                                h = m // 60
                                if h > 0:
                                    self.position_info.rel_time_formatted = f"{h:02d}:{m % 60:02d}:{s:02d}"
                                else:
                                    self.position_info.rel_time_formatted = f"{m:02d}:{s:02d}"
                            elif self.transport_info.state.upper() == "PLAYING" and self.playback_start_time > 0:
                                # Fallback clock timer if stereo reports 00:00:00
                                elapsed = now - self.playback_start_time
                                estimated = self.playback_start_position + elapsed
                                total = (
                                    curr_meta.duration_seconds
                                    if curr_meta and curr_meta.duration_seconds > 0
                                    else p_info.track_duration_seconds
                                )
                                if total > 0:
                                    estimated = min(total, estimated)
                                self.position_info.rel_time_seconds = estimated
                                self.position_info.track_duration_seconds = total
                                self.position_info.track_duration_formatted = (
                                    curr_meta.duration_formatted if curr_meta else p_info.track_duration_formatted
                                )
                                sec = int(estimated)
                                m = sec // 60
                                s = sec % 60
                                h = m // 60
                                if h > 0:
                                    self.position_info.rel_time_formatted = f"{h:02d}:{m % 60:02d}:{s:02d}"
                                else:
                                    self.position_info.rel_time_formatted = f"{m:02d}:{s:02d}"
                            else:
                                self.position_info = p_info

                    if now - last_vol_poll_time >= 2.5:
                        last_vol_poll_time = now
                        vol = self.client.get_volume(timeout=1.2)
                        if vol is not None:
                            self.current_volume = vol

                        mute = self.client.get_mute(timeout=1.2)
                        if mute is not None:
                            self.is_muted = mute

                except Exception as e:
                    logger.debug("Polling exception: %s", e)

            # Render screen
            try:
                self._render(stdscr)
            except Exception as e:
                logger.debug("Render exception: %s", e)

            # Keyboard Input
            try:
                key = stdscr.getch()
            except Exception:
                key = -1

            if key != -1:
                try:
                    self._handle_key(key)
                except Exception as e:
                    logger.error("Exception handling key %s: %s", key, e, exc_info=True)
                    self.set_status(f"Error handling key: {e}")

            if self.pending_seek_delta != 0.0 and (now - self.last_seek_key_time) > 0.6:
                self._flush_pending_seek()

            time.sleep(0.04)

    def _seek_via_stream_offset(self, target: float, delta: float) -> bool:
        """Seek by reloading transport URI with byte/time offset for stereos that reject UPnP Seek."""
        curr_track = self.queue.get_current_track()
        if not curr_track or not self.client:
            return False

        stream_meta = curr_track
        if self.force_transcode or curr_track.format_name == "AIFF":
            trans_meta = self.transcoder.transcode_to_wav(curr_track)
            if trans_meta:
                stream_meta = trans_meta

        stream_url = self.server_manager.get_stream_url(stream_meta, offset_seconds=target)

        # 1. Close existing client sockets on server to abort previous stream cleanly
        srv = getattr(self.server_manager, "server", None)
        if srv and hasattr(srv, "close_all_client_sockets"):
            srv.close_all_client_sockets()

        # 2. Tell stereo to stop previous stream
        try:
            self.client.stop(timeout=1.0)
        except Exception:
            pass

        # 3. Set transport URI with offset query parameter
        if not self.client.set_transport_uri(stream_url, stream_meta):
            return False

        # 4. Start playback
        if not self.client.play():
            return False

        self.transport_info.state = "PLAYING"
        self.playback_stream_offset = target
        self.playback_start_position = target
        self.playback_start_time = time.time()
        self.position_info.rel_time_seconds = target
        sec = int(target)
        m = sec // 60
        s = sec % 60
        h = m // 60
        if h > 0:
            self.position_info.rel_time_formatted = f"{h:02d}:{m % 60:02d}:{s:02d}"
        else:
            self.position_info.rel_time_formatted = f"{m:02d}:{s:02d}"

        sign = "+" if delta > 0 else ""
        self.set_status(f"Seeked {sign}{int(delta)}s to {self.position_info.rel_time_formatted}")
        return True

    def _seek_relative(self, delta: float) -> None:
        """Accumulate seek forward or backward by delta seconds and update UI immediately."""
        if not self.client:
            self.set_status("No stereo connected.")
            return

        self.pending_seek_delta += delta
        self.last_seek_key_time = time.time()
        
        # Visually update the UI right away
        curr_meta = self.queue.get_current_track()
        total = (
            self.position_info.track_duration_seconds
            if self.position_info.track_duration_seconds > 0
            else (curr_meta.duration_seconds if curr_meta else 0.0)
        )
        cur_pos = self.position_info.rel_time_seconds
        target = max(0.0, cur_pos + delta)
        if total > 0:
            target = min(total, target)
        
        self.position_info.rel_time_seconds = target
        sec = int(target)
        m = sec // 60
        s = sec % 60
        h = m // 60
        if h > 0:
            self.position_info.rel_time_formatted = f"{h:02d}:{m % 60:02d}:{s:02d}"
        else:
            self.position_info.rel_time_formatted = f"{m:02d}:{s:02d}"

        sign = "+" if self.pending_seek_delta > 0 else ""
        self.set_status(f"Seeking {sign}{int(self.pending_seek_delta)}s to {self.position_info.rel_time_formatted}...")

    def _flush_pending_seek(self) -> None:
        """Execute the accumulated seek operation."""
        if self.pending_seek_delta == 0.0:
            return
            
        target = self.position_info.rel_time_seconds
        delta = self.pending_seek_delta
        self.pending_seek_delta = 0.0
        
        sign = "+" if delta > 0 else ""
        
        if isinstance(self.client, LocalPlayerClient):
            self.client.seek(target)
            self.playback_start_position = target
            self.playback_start_time = time.time()
            self.set_status(f"Seeked {sign}{int(delta)}s to {self.position_info.rel_time_formatted}")
            return
            
        # Direct server-side stream offset seek (reliable on all DLNA renderers)
        if self._seek_via_stream_offset(target, delta):
            self.set_status(f"Seeked {sign}{int(delta)}s to {self.position_info.rel_time_formatted}")
            return

        err = self.client.last_error or "Seek rejected"
        self.set_status(f"Seek failed: {err}")

    def _handle_key(self, key: int) -> None:
        """Route keyboard shortcut."""
        if key == curses.KEY_RESIZE:
            return

        if key in (ord("?"),):
            self.show_help_modal = not getattr(self, "show_help_modal", False)
            return
            
        if getattr(self, "show_help_modal", False):
            if key in (27, ord("q"), ord("Q")):
                if key == 27:
                    self.show_help_modal = False
                else:
                    self.running = False
            return

        # Global Hotkeys
        if key == ord("R"):
            if self.client and self.client.seek(0):
                self.position_info.rel_time_seconds = 0
                self.position_info.rel_time_formatted = "00:00"
                self.playback_start_position = 0.0
                self.playback_start_time = time.time()
                self.client.play()
                self.set_status("Restarted track")
            else:
                if self.queue.current_index >= 0:
                    self.play_track(self.queue.current_index)
            return

        if key in (ord("t"), ord("T")):
            self.force_transcode = not self.force_transcode
            mode = "16-bit PCM WAV" if self.force_transcode else "Direct Native"
            self.set_status(f"Transcoding: {mode}. Restarting stream...")
            if self.queue.current_index >= 0:
                self.play_track(self.queue.current_index)
            return

        if key in (ord("m"), ord("M")):
            new_mute = not self.is_muted
            if self.client and self.client.set_mute(new_mute):
                self.is_muted = new_mute
                self.set_status("Muted" if new_mute else "Unmuted")
            return

        # Quit
        if key in (ord("q"), ord("Q")):
            self.set_status("Shutting down...")
            self.running = False
            if self.client:
                try:
                    self.client.stop()
                except Exception:
                    pass
            srv = getattr(self.server_manager, "server", None)
            if srv and hasattr(srv, "close_all_client_sockets"):
                try:
                    srv.close_all_client_sockets()
                except Exception:
                    pass
            return

        # Tab Switching
        if key == ord("\t"):  # Tab
            self.active_tab = (self.active_tab + 1) % 3
            return
        elif key in (ord("1"), ord("!")):
            self.active_tab = 0
            return
        elif key in (ord("2"), ord("@")):
            self.active_tab = 1
            return
        elif key in (ord("3"), ord("#")):
            self.active_tab = 2
            return


        # Toggle Metadata / Filename View
        if key in (ord("v"), ord("V")):
            new_val = not self.settings.metadata_view
            self.settings_manager.set_value("metadata_view", new_val)
            self.settings = self.settings_manager.settings
            if self.file_browser:
                self.file_browser.set_metadata_view(new_val)
            mode_desc = "Metadata (Tags)" if new_val else "Filename"
            self.set_status(f"Display mode: {mode_desc}")
            return

        # Global Seek Forward (Shift+→)
        if key in (curses.KEY_SRIGHT, getattr(curses, "KEY_SF", -1)):
            self._seek_relative(self.settings.seek_step_seconds)
            return

        # Global Seek Backward (Shift+←)
        if key in (curses.KEY_SLEFT, getattr(curses, "KEY_SR", -1)):
            self._seek_relative(-self.settings.seek_step_seconds)
            return

        # Global Seek Forward 30s (']')
        if key == ord("]"):
            self._seek_relative(30.0)
            return

        # Global Seek Backward 30s ('[')
        if key == ord("["):
            self._seek_relative(-30.0)
            return

        # Global Next Track (→, 'n', 'N', '>', '.')
        if key in (curses.KEY_RIGHT, ord("n"), ord("N"), ord(">"), ord(".")):
            if len(self.queue) == 0:
                self.set_status("PlaybackQueue is empty. Add media in Tab [2].")
                return

            if len(self.queue) == 1:
                self.set_status("No other tracks in queue.")
                return

            if self.queue.has_next():
                next_idx = self.queue.current_index + 1
                self.play_track(next_idx)
            else:
                self.set_status("Looping to start of queue...")
                self.play_track(0)
            return

        # Global Previous Track (←, 'p', 'P', '<', ',')
        if key in (curses.KEY_LEFT, ord("p"), ord("P"), ord("<"), ord(",")):
            if len(self.queue) == 0:
                self.set_status("PlaybackQueue is empty. Add media in Tab [2].")
                return

            if len(self.queue) == 1:
                self.set_status("No other tracks in queue.")
                return

            if self.queue.current_index > 0:
                if self.position_info.rel_time_seconds > 3.0:
                    self.set_status("Restarting track...")
                    self.play_track(self.queue.current_index)
                else:
                    prev_idx = self.queue.current_index - 1
                    self.play_track(prev_idx)
            else:
                if self.position_info.rel_time_seconds > 3.0:
                    self.set_status("Restarting track...")
                    self.play_track(0)
                else:
                    last_idx = len(self.queue) - 1
                    self.set_status("Wrapping to end of queue...")
                    self.play_track(last_idx)
            return

        # Global Volume Up ('+', '=')
        if key in (ord("+"), ord("=")):
            if self.client:
                step = self.settings.volume_step
                new_vol = min(100, self.current_volume + step)
                if self.client.set_volume(new_vol):
                    self.current_volume = new_vol
                    self.set_status(f"Volume: {new_vol}%")
            return

        # Global Volume Down ('-', '_')
        if key in (ord("-"), ord("_")):
            if self.client:
                step = self.settings.volume_step
                new_vol = max(0, self.current_volume - step)
                if self.client.set_volume(new_vol):
                    self.current_volume = new_vol
                    self.set_status(f"Volume: {new_vol}%")
            return

        # Global Play / Pause Toggle
        if key == ord(" "):
            if not self.client:
                if len(self.queue) > 0:
                    self.play_track(0)
                else:
                    self.set_status("No tracks in queue. Add media in Tab [2].")
                return

            state = self.transport_info.state.upper()
            if "PAUS" in state:
                # 1. Resuming from pause
                self.user_stopped = False
                was_paused_via_stop = getattr(self.client, "is_paused_via_stop", False)
                curr_track = self.queue.get_current_track()
                resume_pos = max(0.0, self.paused_position)

                if was_paused_via_stop and curr_track:
                    # Renderer does not support UPnP Pause (was paused via stop).
                    # Resume by setting transport URI with offset position and issuing Play.
                    stream_meta = curr_track
                    if self.force_transcode or curr_track.format_name == "AIFF":
                        trans_meta = self.transcoder.transcode_to_wav(curr_track)
                        if trans_meta:
                            stream_meta = trans_meta

                    stream_url = self.server_manager.get_stream_url(stream_meta, offset_seconds=resume_pos)

                    srv = getattr(self.server_manager, "server", None)
                    if srv and hasattr(srv, "close_all_client_sockets"):
                        srv.close_all_client_sockets()

                    try:
                        self.client.stop(timeout=1.0)
                    except Exception:
                        pass

                    uri_ok = False
                    for attempt in range(2):
                        if self.client.set_transport_uri(stream_url, stream_meta):
                            uri_ok = True
                            break
                        time.sleep(0.2)

                    play_ok = False
                    if uri_ok:
                        play_ok = self.client.play()

                    if play_ok:
                        self.transport_info.state = "PLAYING"
                        self.playback_stream_offset = resume_pos
                        self.playback_start_position = resume_pos
                        self.playback_start_time = time.time()
                        self.position_info.rel_time_seconds = resume_pos
                        self.client.is_paused_via_stop = False
                        self.set_status(f"Resumed ({self.position_info.rel_time_formatted})")
                    else:
                        err = self.client.last_error or "Resume failed"
                        self.set_status(f"Resume error: {err}")
                else:
                    # Renderer natively paused stream
                    play_ok = self.client.play()
                    if play_ok:
                        self.transport_info.state = "PLAYING"
                        self.playback_start_time = time.time()
                        self.client.is_paused_via_stop = False
                        self.set_status("Playing")
                    else:
                        err = self.client.last_error or "Play rejected"
                        self.set_status(f"Play error: {err}")

            elif "PLAY" in state:
                # 2. Pausing while currently playing
                paused_pos = self.position_info.rel_time_seconds
                if self.client.pause():
                    self.transport_info.state = "PAUSED_PLAYBACK"
                    self.playback_start_position = paused_pos
                    self.playback_start_time = 0.0
                    self.paused_position = paused_pos
                    self.set_status(f"Paused ({self.position_info.rel_time_formatted})")
                else:
                    err = self.client.last_error or "Pause rejected"
                    self.set_status(f"Pause error: {err}")

            else:
                # 3. Starting playback from stopped
                self.user_stopped = False
                if self.queue.current_index >= 0:
                    self.play_track(self.queue.current_index)
                elif len(self.queue) > 0:
                    self.play_track(0)
            return

        # Global Stop
        if key in (ord("s"), ord("S")) and self.active_tab != 1:
            self.user_stopped = True
            if self.client:
                try:
                    self.client.stop()
                    self.client.is_paused_via_stop = False
                except Exception as e:
                    logger.warning("Stop exception: %s", e)
            srv = getattr(self.server_manager, "server", None)
            if srv and hasattr(srv, "close_all_client_sockets"):
                try:
                    srv.close_all_client_sockets()
                except Exception:
                    pass
            self.transport_info.state = "STOPPED"
            self.position_info.rel_time_seconds = 0.0
            self.position_info.rel_time_formatted = "00:00"
            self.playback_start_position = 0.0
            self.playback_start_time = 0.0
            self.paused_position = 0.0
            self.set_status("Playback stopped")
            return

        # 3. Tab-Specific Handlers
        if self.active_tab == 0:
            self._handle_queue_keys(key)
        elif self.active_tab == 1:
            self._handle_device_keys(key)
        elif self.active_tab == 2:
            self._handle_settings_keys(key)

    def _handle_queue_keys(self, key: int) -> None:
        """Keys for Tab 1: Queue & Media Manager."""
        max_idx = max(0, len(self.queue) - 1)

        # Focus Switching (fallback to h/l since arrows are next/prev track)
        if key in (ord("h"), ord("H")):
            self.queue_focus = "browser"
            return
        elif key in (ord("l"), ord("L")):
            self.queue_focus = "queue"
            return

        if self.queue_focus == "browser":
            if self.file_browser is not None:
                should_close, added, msg = self.file_browser.handle_key(key)
                if msg:
                    self.set_status(msg)
                if added:
                    for path in added:
                        if path.is_dir():
                            added_tracks = self.queue.add_directory(path)
                            self.set_status(f"Added {len(added_tracks)} tracks")
                        else:
                            self.queue.add_file(path)
            return

        # Queue Focus Navigation
        # Navigate Up
        if key in (curses.KEY_UP, ord("k"), ord("K")):
            if len(self.queue) > 0:
                if self.queue_cursor > 0:
                    self.queue_cursor -= 1
                else:
                    self.queue_cursor = max_idx

        # Navigate Down
        elif key in (curses.KEY_DOWN, ord("j"), ord("J")):
            if len(self.queue) > 0:
                if self.queue_cursor < max_idx:
                    self.queue_cursor += 1
                else:
                    self.queue_cursor = 0

        # Page Up
        elif key in (getattr(curses, "KEY_PPAGE", 339), 21, 2, ord("K")):
            step = 15
            if len(self.queue) > 0:
                if self.queue_cursor >= step:
                    self.queue_cursor -= step
                elif self.queue_cursor > 0:
                    self.queue_cursor = 0
                else:
                    self.queue_cursor = max_idx

        # Page Down
        elif key in (getattr(curses, "KEY_NPAGE", 338), 4, 6, ord("J")):
            step = 15
            if len(self.queue) > 0:
                if self.queue_cursor + step <= max_idx:
                    self.queue_cursor += step
                elif self.queue_cursor < max_idx:
                    self.queue_cursor = max_idx
                else:
                    self.queue_cursor = 0

        # Home
        elif key in (getattr(curses, "KEY_HOME", -1), ord("g")):
            if len(self.queue) > 0:
                self.queue_cursor = 0

        # End
        elif key in (getattr(curses, "KEY_END", -1), ord("G")):
            if len(self.queue) > 0:
                self.queue_cursor = max_idx

        # Play highlighted track
        elif key in (curses.KEY_ENTER, 10, 13):
            if 0 <= self.queue_cursor < len(self.queue):
                self.play_track(self.queue_cursor)

        # Delete track from queue
        elif key in (ord("d"), ord("D"), ord("x"), ord("X"), curses.KEY_DC):
            if 0 <= self.queue_cursor < len(self.queue):
                removed = self.queue.remove(self.queue_cursor)
                if removed:
                    self.set_status(f"Removed '{removed.display_title}'")
                if self.queue_cursor >= len(self.queue):
                    self.queue_cursor = max(0, len(self.queue) - 1)

        # Clear queue
        elif key in (ord("c"), ord("C")):
            self.queue.clear()
            self.queue_cursor = 0
            self.queue_offset = 0
            self.set_status("Queue cleared")

    def _handle_device_keys(self, key: int) -> None:
        """Keys for Tab 2: Devices."""
        devices = self.device_manager.devices
        max_idx = max(0, len(devices) - 1)

        # Navigate Up
        if key in (curses.KEY_UP, ord("k"), ord("K")):
            if len(devices) > 0:
                if self.device_cursor > 0:
                    self.device_cursor -= 1
                else:
                    self.device_cursor = max_idx

        # Navigate Down
        elif key in (curses.KEY_DOWN, ord("j"), ord("J")):
            if len(devices) > 0:
                if self.device_cursor < max_idx:
                    self.device_cursor += 1
                else:
                    self.device_cursor = 0

        # Connect to highlighted device
        elif key in (curses.KEY_ENTER, 10, 13):
            if 0 <= self.device_cursor < len(devices):
                dev = devices[self.device_cursor]
                self.device_manager.set_active_device(dev)
                if self.client:
                    try:
                        self.client.stop()
                    except Exception:
                        pass
                if dev.udn == "local-audio":
                    self.client = LocalPlayerClient(dev)
                else:
                    self.client = UPnPClient(dev)
                self.set_status(f"Connected to {dev.friendly_name}")

        # Rescan network
        elif key in (ord("s"), ord("S")):
            self.set_status("Scanning local network for stereos...")
            self.device_manager.start_scan(
                timeout=3.5,
                on_complete=lambda devs: self.set_status(f"Scan complete: found {len(devs)} stereos"),
            )

    def _handle_settings_keys(self, key: int) -> None:
        """Keys for Tab 3: Settings."""
        defs = self.settings_manager.definitions
        max_idx = len(defs) - 1

        # Move cursor up
        if key in (curses.KEY_UP, ord("k"), ord("K")):
            if self.settings_cursor > 0:
                self.settings_cursor -= 1
            else:
                self.settings_cursor = max_idx

        # Move cursor down
        elif key in (curses.KEY_DOWN, ord("j"), ord("J")):
            if self.settings_cursor < max_idx:
                self.settings_cursor += 1
            else:
                self.settings_cursor = 0

        # Cycle previous option
        elif key in (curses.KEY_LEFT, ord("h"), ord("H")):
            self._cycle_and_apply_setting(forward=False)

        # Cycle next option
        elif key in (curses.KEY_RIGHT, ord("l"), ord("L"), curses.KEY_ENTER, 10, 13, ord(" ")):
            self._cycle_and_apply_setting(forward=True)

        # Reset defaults
        elif key in (ord("r"), ord("R")):
            self.settings = self.settings_manager.reset_defaults()
            self._apply_all_settings()
            self.set_status("All settings reset to defaults")

    def _cycle_and_apply_setting(self, forward: bool = True) -> None:
        """Cycle the active setting and apply immediately to runtime."""
        defn = self.settings_manager.definitions[self.settings_cursor]
        self.settings_manager.cycle_setting(self.settings_cursor, forward=forward)
        self.settings = self.settings_manager.settings
        self._apply_setting(defn.key)
        new_label = self.settings_manager.get_display_value(self.settings_cursor)
        self.set_status(f"Set {defn.label}: {new_label}")

    def _apply_setting(self, key: str) -> None:
        """Apply a single setting change immediately."""
        if key == "metadata_view":
            if self.file_browser:
                self.file_browser.set_metadata_view(self.settings.metadata_view)
        elif key == "force_transcode":
            self.force_transcode = self.settings.force_transcode
            mode = "16-bit PCM WAV" if self.force_transcode else "Direct Native"
            if self.queue.current_index >= 0 and self.transport_info.state == "PLAYING":
                self.play_track(self.queue.current_index)

    def _apply_all_settings(self) -> None:
        """Apply all settings to active components."""
        if self.file_browser:
            self.file_browser.set_metadata_view(self.settings.metadata_view)
        self.force_transcode = self.settings.force_transcode
