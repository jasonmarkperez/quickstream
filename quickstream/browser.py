import curses
import json
from pathlib import Path
import threading
from typing import Dict, List, Optional, Set, Tuple

from quickstream.metadata import AudioMetadata, extract_metadata, is_supported_audio_file
from quickstream.paths import get_bookmarks_path


def load_bookmarks(path: Optional[Path] = None) -> List[str]:
    bm_path = path or get_bookmarks_path()
    if bm_path.exists():
        try:
            with open(bm_path, "r", encoding="utf-8") as f:
                return json.load(f)
        except Exception:
            pass
    return []


def save_bookmarks(bookmarks: List[str], path: Optional[Path] = None) -> None:
    bm_path = path or get_bookmarks_path()
    try:
        bm_path.parent.mkdir(parents=True, exist_ok=True)
        with open(bm_path, "w", encoding="utf-8") as f:
            json.dump(bookmarks, f, indent=2)
    except Exception:
        pass


class FileBrowser:
    """In-app file and directory browser with persistent bookmarks and metadata view."""

    def __init__(
        self,
        start_dir: Optional[Path] = None,
        metadata_view: bool = True,
        bookmarks_path: Optional[Path] = None,
    ):
        self.current_dir = (start_dir or Path.cwd()).resolve()
        self.entries: List[Tuple[str, Path, bool]] = []
        self.cursor_idx = 0
        self.scroll_offset = 0
        self.bookmarks_path = bookmarks_path
        self.bookmarks = load_bookmarks(self.bookmarks_path)
        self.in_bookmarks_view = False
        self.metadata_view = metadata_view
        self.metadata_cache: Dict[str, AudioMetadata] = {}
        self._fetching_paths: Set[str] = set()
        self._fetch_lock = threading.Lock()
        self._refresh_entries()

    def _save_bookmarks(self) -> None:
        save_bookmarks(self.bookmarks, self.bookmarks_path)

    def set_metadata_view(self, enabled: bool) -> None:
        """Toggle or set metadata display mode and update labels."""
        if self.metadata_view != enabled:
            self.metadata_view = enabled
            self._update_entry_labels()
            if self.metadata_view:
                self._trigger_metadata_extraction()

    def _format_audio_label(self, path: Path) -> str:
        """Format display label for an audio file depending on metadata_view."""
        ext = path.suffix.upper().lstrip(".")
        if self.metadata_view and str(path) in self.metadata_cache:
            meta = self.metadata_cache[str(path)]
            artist_part = f"{meta.artist} - " if meta.artist else ""
            time_part = f" ({meta.duration_formatted})" if meta.duration_seconds > 0 else ""
            title = meta.display_title
            return f"[{ext}] {artist_part}{title}{time_part}"
        return f"[{ext}] {path.name}"

    def _update_entry_labels(self) -> None:
        """Update display strings for all entries in the current listing."""
        new_entries: List[Tuple[str, Path, bool]] = []
        for name, path, is_dir in self.entries:
            if is_dir:
                new_entries.append((name, path, is_dir))
            else:
                new_entries.append((self._format_audio_label(path), path, False))
        self.entries = new_entries

    def _trigger_metadata_extraction(self) -> None:
        """Asynchronously extract audio metadata for files in current view."""
        if not self.metadata_view or self.in_bookmarks_view:
            return

        unfetched: List[Path] = []
        with self._fetch_lock:
            for _, path, is_dir in self.entries:
                if not is_dir and str(path) not in self.metadata_cache and str(path) not in self._fetching_paths:
                    unfetched.append(path)
                    self._fetching_paths.add(str(path))

        if not unfetched:
            return

        def _worker():
            for p in unfetched:
                try:
                    meta = extract_metadata(p)
                    self.metadata_cache[str(p)] = meta
                except Exception:
                    pass
                finally:
                    with self._fetch_lock:
                        self._fetching_paths.discard(str(p))
            # Refresh entry labels with newly fetched metadata
            self._update_entry_labels()

        threading.Thread(target=_worker, daemon=True, name="MetadataExtractor").start()

    def _refresh_entries(self) -> None:
        """Scan current directory or bookmarks and schedule metadata fetching."""
        self.entries = []

        if self.in_bookmarks_view:
            self.entries.append((".. [Back to File System]", self.current_dir, True))
            for b in self.bookmarks:
                b_path = Path(b)
                if b_path.is_dir():
                    self.entries.append((f"📁 {b_path.name}/", b_path, True))
                else:
                    self.entries.append((f"📁 {b_path.name} (Not found)/", b_path, True))
            self.cursor_idx = 0
            self.scroll_offset = 0
            return

        try:
            self.entries.append(("★ Music Homes", Path("___MUSIC_HOMES___"), True))

            if self.current_dir.parent != self.current_dir:
                self.entries.append((".. [Parent Directory]", self.current_dir.parent, True))

            dirs: List[Tuple[str, Path, bool]] = []
            files: List[Tuple[str, Path, bool]] = []

            for p in sorted(self.current_dir.iterdir(), key=lambda x: x.name.lower()):
                if p.name.startswith("."):
                    continue
                if p.is_dir():
                    dirs.append((f"📁 {p.name}/", p, True))
                elif p.is_file() and is_supported_audio_file(p):
                    files.append((self._format_audio_label(p), p, False))

            self.entries.extend(dirs)
            self.entries.extend(files)
        except Exception:
            self.entries = [
                ("★ Music Homes", Path("___MUSIC_HOMES___"), True),
                (".. [Parent Directory]", self.current_dir.parent, True),
            ]

        self.cursor_idx = 0
        self.scroll_offset = 0
        self._trigger_metadata_extraction()

    def navigate_into(self, path: Path) -> None:
        """Change current directory and reload entries."""
        if path.name == "___MUSIC_HOMES___":
            self.in_bookmarks_view = True
            self._refresh_entries()
            return

        if self.in_bookmarks_view:
            self.in_bookmarks_view = False

        if path.is_dir():
            self.current_dir = path.resolve()
            self._refresh_entries()

    def render(self, stdscr: curses.window, start_y: int, start_x: int, height: int, width: int, is_focused: bool) -> None:
        """Render file browser view in the given bounding box."""
        cyan = curses.color_pair(1)
        green = curses.color_pair(2)
        yellow = curses.color_pair(3)

        base_attr = curses.A_NORMAL if is_focused else curses.A_DIM
        bold = curses.A_BOLD if is_focused else curses.A_NORMAL

        # Clear background
        for y in range(start_y, start_y + height):
            try:
                stdscr.addstr(y, start_x, " " * width)
            except curses.error:
                pass

        # Title with mode indicator
        mode_tag = " [TAGS]" if self.metadata_view else " [FILES]"
        title = f" BROWSER{mode_tag} " if is_focused else f" browser{mode_tag} "
        try:
            stdscr.attron(cyan | bold)
            stdscr.addstr(start_y, start_x + 1, title)
            stdscr.attroff(cyan | bold)
        except curses.error:
            pass

        # Path info
        count_str = f" ({self.cursor_idx + 1}/{len(self.entries)})" if self.entries else ""
        if self.in_bookmarks_view:
            path_str = f"Location: ★ Music Homes{count_str}"
        else:
            path_str = f"Location: {self.current_dir.name}{count_str}"

        try:
            stdscr.addstr(start_y + 1, start_x + 1, path_str[: width - 2], curses.A_DIM)
            stdscr.addstr(start_y + 2, start_x + 1, "─" * min(width - 2, 76), curses.A_DIM)
        except curses.error:
            pass

        # List
        list_height = max(1, height - 5)
        self.last_list_height = list_height

        # Ensure cursor is visible
        if self.cursor_idx < self.scroll_offset:
            self.scroll_offset = self.cursor_idx
        elif self.cursor_idx >= self.scroll_offset + list_height:
            self.scroll_offset = self.cursor_idx - list_height + 1

        visible_entries = self.entries[self.scroll_offset : self.scroll_offset + list_height]

        if not self.entries:
            try:
                stdscr.addstr(start_y + 4, start_x + 1, "(No files found)", yellow | base_attr)
            except curses.error:
                pass
        else:
            for idx, (name, path, is_dir) in enumerate(visible_entries):
                actual_idx = self.scroll_offset + idx
                row = start_y + 3 + idx
                if row >= start_y + height - 2:
                    break

                is_selected = (actual_idx == self.cursor_idx) and is_focused

                prefix = " > " if is_selected else "   "
                display_line = f"{prefix}{name}"[: width - 2]

                try:
                    if is_selected:
                        stdscr.attron(bold | curses.A_REVERSE)
                        stdscr.addstr(row, start_x, display_line.ljust(width))
                        stdscr.attroff(bold | curses.A_REVERSE)
                    elif is_dir:
                        stdscr.addstr(row, start_x + 1, display_line[1:], bold | cyan | base_attr)
                    else:
                        stdscr.addstr(row, start_x + 1, display_line[1:], green | base_attr)
                except curses.error:
                    pass

        # Footer controls
        footer_row = start_y + height - 2
        try:
            stdscr.addstr(footer_row, start_x + 1, "─" * min(width - 2, 76), curses.A_DIM)
            if self.in_bookmarks_view:
                controls = "[PgDn/PgUp] Page  [Enter] Open  [d/x] Remove  [r] Refresh"
            else:
                is_bm = str(self.current_dir) in self.bookmarks
                bm_label = "[b] Unbookmark" if is_bm else "[b] Bookmark"
                controls = f"[PgDn/PgUp] Page  [Enter] Open/Add  [v] Mode  {bm_label}  [a] All"
            stdscr.addstr(footer_row + 1, start_x + 1, controls[: width - 2], base_attr)
        except curses.error:
            pass

    def page_up(self, step: int = 15) -> None:
        """Page up through entries."""
        if not self.entries:
            return
        if self.cursor_idx >= step:
            self.cursor_idx -= step
        elif self.cursor_idx > 0:
            self.cursor_idx = 0
        else:
            self.cursor_idx = len(self.entries) - 1

    def page_down(self, step: int = 15) -> None:
        """Page down through entries."""
        if not self.entries:
            return
        max_idx = len(self.entries) - 1
        if self.cursor_idx + step <= max_idx:
            self.cursor_idx += step
        elif self.cursor_idx < max_idx:
            self.cursor_idx = max_idx
        else:
            self.cursor_idx = 0

    def home(self) -> None:
        """Jump to first entry."""
        if self.entries:
            self.cursor_idx = 0

    def end(self) -> None:
        """Jump to last entry."""
        if self.entries:
            self.cursor_idx = len(self.entries) - 1

    def handle_key(self, key: int) -> Tuple[bool, Optional[List[Path]], Optional[str]]:
        """
        Handle key input.
        Returns: (should_close, added_paths_or_none, message_or_none)
        """
        max_idx = max(0, len(self.entries) - 1)

        # Navigation Up
        if key in (curses.KEY_UP, ord("k")):
            if len(self.entries) > 0:
                if self.cursor_idx > 0:
                    self.cursor_idx -= 1
                else:
                    self.cursor_idx = max_idx

        # Navigation Down
        elif key in (curses.KEY_DOWN, ord("j")):
            if len(self.entries) > 0:
                if self.cursor_idx < max_idx:
                    self.cursor_idx += 1
                else:
                    self.cursor_idx = 0

        # Page Up
        elif key in (getattr(curses, "KEY_PPAGE", 339), 21, 2, ord("K")):
            step = getattr(self, "last_list_height", 15)
            self.page_up(step)
            return False, None, None

        # Page Down
        elif key in (getattr(curses, "KEY_NPAGE", 338), 4, 6, ord("J")):
            step = getattr(self, "last_list_height", 15)
            self.page_down(step)
            return False, None, None

        # Home
        elif key in (getattr(curses, "KEY_HOME", -1), ord("g")):
            self.home()
            return False, None, None

        # End
        elif key in (getattr(curses, "KEY_END", -1), ord("G")):
            self.end()
            return False, None, None

        # Toggle Metadata / Filename View
        elif key in (ord("v"), ord("V")):
            self.set_metadata_view(not self.metadata_view)
            mode_desc = "Metadata (Tags)" if self.metadata_view else "Filename"
            return False, None, f"Browser display: {mode_desc}"

        # Refresh
        elif key in (ord("r"), ord("R")):
            self._refresh_entries()
            return False, None, "Refreshed folder"

        # Bookmark / Unbookmark toggle
        elif key in (ord("b"), ord("B")):
            if not self.in_bookmarks_view:
                path_str = str(self.current_dir)
                if path_str in self.bookmarks:
                    self.bookmarks.remove(path_str)
                    self._save_bookmarks()
                    return False, None, f"Removed '{self.current_dir.name}' from Music Homes"
                else:
                    self.bookmarks.append(path_str)
                    self._save_bookmarks()
                    return False, None, f"Added '{self.current_dir.name}' to Music Homes"
            else:
                if self.cursor_idx > 0:
                    b_idx = self.cursor_idx - 1
                    if 0 <= b_idx < len(self.bookmarks):
                        removed_str = self.bookmarks.pop(b_idx)
                        self._save_bookmarks()
                        self._refresh_entries()
                        if self.cursor_idx >= len(self.entries):
                            self.cursor_idx = max(0, len(self.entries) - 1)
                        return False, None, f"Removed '{Path(removed_str).name}' from Music Homes"
                return False, None, "Select a bookmark to remove"

        # Remove bookmark with d/x/Delete in Music Homes view
        elif key in (ord("d"), ord("D"), ord("x"), ord("X"), curses.KEY_DC):
            if self.in_bookmarks_view:
                if self.cursor_idx > 0:
                    b_idx = self.cursor_idx - 1
                    if 0 <= b_idx < len(self.bookmarks):
                        removed_str = self.bookmarks.pop(b_idx)
                        self._save_bookmarks()
                        self._refresh_entries()
                        if self.cursor_idx >= len(self.entries):
                            self.cursor_idx = max(0, len(self.entries) - 1)
                        return False, None, f"Removed '{Path(removed_str).name}' from Music Homes"
                return False, None, "Select a bookmark to remove"

        # Add all in folder
        elif key in (ord("a"), ord("A")):
            if self.in_bookmarks_view:
                return False, None, "Cannot add all from Music Homes"
            audio_files = [p for _, p, is_dir in self.entries if not is_dir]
            if audio_files:
                return False, audio_files, f"Added {len(audio_files)} tracks"
            return False, None, "No audio files to add in this folder"

        # Enter / Select
        elif key in (curses.KEY_ENTER, 10, 13):
            if 0 <= self.cursor_idx < len(self.entries):
                _, path, is_dir = self.entries[self.cursor_idx]
                if is_dir:
                    self.navigate_into(path)
                    return False, None, None
                else:
                    return False, [path], f"Added {path.name}"

        return False, None, None
