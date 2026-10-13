"""Automated unit tests for FileBrowser navigation, metadata view, and bookmark management."""

import curses
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from quickstream.browser import FileBrowser, load_bookmarks, save_bookmarks


class TestFileBrowser(unittest.TestCase):
    """Test FileBrowser navigation, bookmark addition/removal, and metadata toggling."""

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name).resolve()

        # Create sample folder hierarchy
        self.folder_a = self.root / "Album_A"
        self.folder_a.mkdir()
        (self.folder_a / "track1.mp3").write_bytes(b"dummy")
        (self.folder_a / "track2.flac").write_bytes(b"dummy")

        self.folder_b = self.root / "Album_B"
        self.folder_b.mkdir()

        # Mock bookmarks file path to temp directory
        self.bm_path = self.root / "bookmarks.json"
        self.bm_patcher = mock.patch("quickstream.browser.get_bookmarks_path", return_value=self.bm_path)
        self.bm_patcher.start()

        self.browser = FileBrowser(start_dir=self.root, metadata_view=False)

    def tearDown(self):
        self.bm_patcher.stop()
        self.temp_dir.cleanup()

    def test_initial_entries(self):
        """Verify root directory contains Music Homes, folders, and proper sorting."""
        names = [entry[0] for entry in self.browser.entries]
        self.assertTrue(any("★ Music Homes" in n for n in names))
        self.assertTrue(any("Album_A" in n for n in names))
        self.assertTrue(any("Album_B" in n for n in names))

    def test_bookmark_add_and_unbookmark_current_dir(self):
        """Verify [b] toggles bookmarking for current directory."""
        self.browser.navigate_into(self.folder_a)
        self.assertEqual(self.browser.bookmarks, [])

        # Add bookmark
        should_close, added, msg = self.browser.handle_key(ord("b"))
        self.assertFalse(should_close)
        self.assertIn("Added", msg)
        self.assertIn(str(self.folder_a), self.browser.bookmarks)

        # Pressing [b] again unbookmarks the current folder
        should_close, added, msg = self.browser.handle_key(ord("b"))
        self.assertIn("Removed", msg)
        self.assertNotIn(str(self.folder_a), self.browser.bookmarks)

    def test_music_homes_navigation_and_removal(self):
        """Verify entering Music Homes and removing bookmarks using 'd', 'x', and 'b'."""
        self.browser.bookmarks = [str(self.folder_a), str(self.folder_b)]
        save_bookmarks(self.browser.bookmarks)

        # Navigate into Music Homes
        self.browser.navigate_into(Path("___MUSIC_HOMES___"))
        self.assertTrue(self.browser.in_bookmarks_view)
        # Entry 0 = .. [Back to File System], Entry 1 = Album_A, Entry 2 = Album_B
        self.assertEqual(len(self.browser.entries), 3)

        # Attempting to delete entry 0 (Back to File System) is ignored
        self.browser.cursor_idx = 0
        _, _, msg = self.browser.handle_key(ord("d"))
        self.assertEqual(msg, "Select a bookmark to remove")
        self.assertEqual(len(self.browser.bookmarks), 2)

        # Select Entry 1 (Album_A) and remove with 'd'
        self.browser.cursor_idx = 1
        _, _, msg = self.browser.handle_key(ord("d"))
        self.assertIn("Removed 'Album_A'", msg)
        self.assertEqual(self.browser.bookmarks, [str(self.folder_b)])
        self.assertEqual(len(self.browser.entries), 2)  # Back + Album_B

        # Select Entry 1 (Album_B) and remove with 'x'
        self.browser.cursor_idx = 1
        _, _, msg = self.browser.handle_key(ord("x"))
        self.assertIn("Removed 'Album_B'", msg)
        self.assertEqual(self.browser.bookmarks, [])
        self.assertEqual(len(self.browser.entries), 1)  # Only Back button remains

    def test_remove_bookmark_with_b_inside_music_homes(self):
        """Verify 'b' also removes selected bookmark when inside Music Homes."""
        self.browser.bookmarks = [str(self.folder_a)]
        self.browser.navigate_into(Path("___MUSIC_HOMES___"))
        self.browser.cursor_idx = 1  # Select Album_A

        _, _, msg = self.browser.handle_key(ord("b"))
        self.assertIn("Removed 'Album_A'", msg)
        self.assertEqual(self.browser.bookmarks, [])

    def test_toggle_metadata_view(self):
        """Verify 'v' key toggles metadata_view attribute and updates labels."""
        self.browser.navigate_into(self.folder_a)
        self.assertFalse(self.browser.metadata_view)

        _, _, msg = self.browser.handle_key(ord("v"))
        self.assertTrue(self.browser.metadata_view)
        self.assertIn("Metadata", msg)

        _, _, msg = self.browser.handle_key(ord("v"))
        self.assertFalse(self.browser.metadata_view)
        self.assertIn("Filename", msg)

    def test_browser_paging_and_jumping(self):
        """Verify paging through entries via PageDown/PageUp, Ctrl+D/U, J/K, and Home/End."""
        # Create a folder with 40 files
        paged_dir = self.root / "Paged_Folder"
        paged_dir.mkdir()
        for i in range(40):
            (paged_dir / f"track_{i:02d}.mp3").write_bytes(b"dummy")

        self.browser.navigate_into(paged_dir)
        total_entries = len(self.browser.entries)
        self.assertGreaterEqual(total_entries, 40)
        self.browser.last_list_height = 10

        self.assertEqual(self.browser.cursor_idx, 0)

        # 1. Page Down via KEY_NPAGE jumps by 10
        self.browser.handle_key(curses.KEY_NPAGE)
        self.assertEqual(self.browser.cursor_idx, 10)

        # 2. Ctrl+D (code 4) jumps by 10
        self.browser.handle_key(4)
        self.assertEqual(self.browser.cursor_idx, 20)

        # 3. Shift+J (ord('J')) jumps by 10
        self.browser.handle_key(ord("J"))
        self.assertEqual(self.browser.cursor_idx, 30)

        # 4. End ('G') jumps to last entry
        self.browser.handle_key(ord("G"))
        self.assertEqual(self.browser.cursor_idx, total_entries - 1)

        # 5. Page Down at last entry wraps to 0
        self.browser.handle_key(curses.KEY_NPAGE)
        self.assertEqual(self.browser.cursor_idx, 0)

        # 6. Page Up at 0 wraps to last entry
        self.browser.handle_key(curses.KEY_PPAGE)
        self.assertEqual(self.browser.cursor_idx, total_entries - 1)

        # 7. Ctrl+U (code 21) jumps up by 10
        self.browser.handle_key(21)
        self.assertEqual(self.browser.cursor_idx, total_entries - 1 - 10)

        # 8. Shift+K (ord('K')) jumps up by 10
        self.browser.handle_key(ord("K"))
        self.assertEqual(self.browser.cursor_idx, total_entries - 1 - 20)

        # 9. Home ('g') jumps to 0
        self.browser.handle_key(ord("g"))
        self.assertEqual(self.browser.cursor_idx, 0)


if __name__ == "__main__":
    unittest.main()

