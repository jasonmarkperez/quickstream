import json
import curses
from pathlib import Path
from typing import List, Optional, Tuple
from quickstream.metadata import is_supported_audio_file

def load_bookmarks():
    path = Path.home() / ".quickstream_bookmarks.json"
    if path.exists():
        try:
            with open(path, "r") as f:
                return json.load(f)
        except Exception:
            pass
    return []

def save_bookmarks(bookmarks):
    path = Path.home() / ".quickstream_bookmarks.json"
    with open(path, "w") as f:
        json.dump(bookmarks, f)

class FileBrowser:
    """In-app file and directory browser for selecting audio media."""

    def __init__(self, start_dir: Optional[Path] = None):
        self.current_dir = (start_dir or Path.cwd()).resolve()
        self.entries: List[Tuple[str, Path, bool]] = []
        self.cursor_idx = 0
        self.scroll_offset = 0
        self.bookmarks = load_bookmarks()
        self.in_bookmarks_view = False
        self._refresh_entries()

    def _refresh_entries(self) -> None:
        """Scan current directory or bookmarks."""
        self.entries = []
        
        if self.in_bookmarks_view:
            self.entries.append((".. [Back to File System]", self.current_dir, True))
            for b in self.bookmarks:
                b_path = Path(b)
                if b_path.is_dir():
                    self.entries.append((f"📁 {b_path.name}/", b_path, True))
                else:
                    # Stale bookmark? Just show it
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
                    ext = p.suffix.upper().lstrip(".")
                    files.append((f"🎵 [{ext}] {p.name}", p, False))

            self.entries.extend(dirs)
            self.entries.extend(files)
        except Exception:
            self.entries = [
                ("★ Music Homes", Path("___MUSIC_HOMES___"), True),
                (".. [Parent Directory]", self.current_dir.parent, True)
            ]

        self.cursor_idx = 0
        self.scroll_offset = 0

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
        magenta = curses.color_pair(5)
        
        # Determine attributes based on focus
        base_attr = curses.A_NORMAL if is_focused else curses.A_DIM
        bold = curses.A_BOLD if is_focused else curses.A_NORMAL
        
        # Clear background
        for y in range(start_y, start_y + height):
            try:
                stdscr.addstr(y, start_x, " " * width)
            except curses.error:
                pass

        # Title
        title = " BROWSER " if is_focused else " browser "
        try:
            stdscr.attron(cyan | bold)
            stdscr.addstr(start_y, start_x + 1, title)
            stdscr.attroff(cyan | bold)
        except curses.error:
            pass

        # Path info
        if self.in_bookmarks_view:
            path_str = "Location: ★ Music Homes"
        else:
            path_str = f"Location: {self.current_dir.name}"
            
        try:
            stdscr.addstr(start_y + 1, start_x + 1, path_str[: width - 2], curses.A_DIM)
            stdscr.addstr(start_y + 2, start_x + 1, "─" * min(width - 2, 76), curses.A_DIM)
        except curses.error:
            pass

        # List
        list_height = max(1, height - 5)
        
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
            controls = "[Enter] Open/Add  [b] Bookmark  [a] Add All"
            stdscr.addstr(footer_row + 1, start_x + 1, controls[: width - 2], base_attr)
        except curses.error:
            pass

    def handle_key(self, key: int) -> Tuple[bool, Optional[List[Path]], Optional[str]]:
        """
        Handle key input.
        Returns: (should_close, added_paths_or_none, message_or_none)
        """
        max_idx = max(0, len(self.entries) - 1)

        # Navigation Up
        if key in (curses.KEY_UP, ord("k"), ord("K")):
            if len(self.entries) > 0:
                if self.cursor_idx > 0:
                    self.cursor_idx -= 1
                else:
                    self.cursor_idx = max_idx

        # Navigation Down
        elif key in (curses.KEY_DOWN, ord("j"), ord("J")):
            if len(self.entries) > 0:
                if self.cursor_idx < max_idx:
                    self.cursor_idx += 1
                else:
                    self.cursor_idx = 0

        # Refresh
        elif key in (ord("r"), ord("R")):
            self._refresh_entries()
            return False, None, "Refreshed folder"
            
        # Bookmark
        elif key in (ord("b"), ord("B")):
            if not self.in_bookmarks_view:
                path_str = str(self.current_dir)
                if path_str not in self.bookmarks:
                    self.bookmarks.append(path_str)
                    save_bookmarks(self.bookmarks)
                    return False, None, "Added to Music Homes"
                else:
                    return False, None, "Already in Music Homes"

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
