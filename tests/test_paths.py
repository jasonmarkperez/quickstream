"""Automated unit tests for XDG path resolution and legacy file migration."""

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from quickstream.paths import (
    _migrate_file,
    get_bookmarks_path,
    get_cache_dir,
    get_config_dir,
    get_config_path,
    get_data_dir,
)


class TestPaths(unittest.TestCase):
    """Test XDG directory standards, environment variable overrides, and backward-compatible migration."""

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.mock_home = Path(self.temp_dir.name).resolve()
        self.home_patcher = mock.patch("quickstream.paths.Path.home", return_value=self.mock_home)
        self.home_patcher.start()

    def tearDown(self):
        self.home_patcher.stop()
        self.temp_dir.cleanup()

    def test_default_config_dir(self):
        """Verify default config dir resolves to ~/.config/quickstream."""
        with mock.patch.dict(os.environ, {}, clear=True):
            expected = self.mock_home / ".config" / "quickstream"
            self.assertEqual(get_config_dir(), expected)

    def test_xdg_config_home_override(self):
        """Verify XDG_CONFIG_HOME overrides default config dir."""
        custom_base = self.mock_home / "custom_config"
        with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(custom_base)}):
            expected = custom_base / "quickstream"
            self.assertEqual(get_config_dir(), expected)

    def test_default_cache_dir(self):
        """Verify default cache dir resolves to ~/.cache/quickstream."""
        with mock.patch.dict(os.environ, {}, clear=True):
            expected = self.mock_home / ".cache" / "quickstream"
            self.assertEqual(get_cache_dir(), expected)

    def test_xdg_cache_home_override(self):
        """Verify XDG_CACHE_HOME overrides default cache dir."""
        custom_cache = self.mock_home / "custom_cache"
        with mock.patch.dict(os.environ, {"XDG_CACHE_HOME": str(custom_cache)}):
            expected = custom_cache / "quickstream"
            self.assertEqual(get_cache_dir(), expected)

    def test_default_data_dir(self):
        """Verify default data dir resolves to ~/.local/share/quickstream."""
        with mock.patch.dict(os.environ, {}, clear=True):
            expected = self.mock_home / ".local" / "share" / "quickstream"
            self.assertEqual(get_data_dir(), expected)

    def test_xdg_data_home_override(self):
        """Verify XDG_DATA_HOME overrides default data dir."""
        custom_data = self.mock_home / "custom_data"
        with mock.patch.dict(os.environ, {"XDG_DATA_HOME": str(custom_data)}):
            expected = custom_data / "quickstream"
            self.assertEqual(get_data_dir(), expected)

    def test_config_migration_when_legacy_exists(self):
        """Verify ~/.quickstream_config.json is migrated to ~/.config/quickstream/config.json."""
        with mock.patch.dict(os.environ, {}, clear=True):
            legacy_file = self.mock_home / ".quickstream_config.json"
            sample_data = {"metadata_view": False, "volume_step": 10}
            legacy_file.write_text(json.dumps(sample_data), encoding="utf-8")

            target_file = get_config_path(migrate=True)
            expected_target = self.mock_home / ".config" / "quickstream" / "config.json"

            self.assertEqual(target_file, expected_target)
            self.assertTrue(expected_target.exists())
            self.assertEqual(json.loads(expected_target.read_text(encoding="utf-8")), sample_data)
            # Legacy file is preserved as fallback
            self.assertTrue(legacy_file.exists())

    def test_bookmarks_migration_when_legacy_exists(self):
        """Verify ~/.quickstream_bookmarks.json is migrated to ~/.config/quickstream/bookmarks.json."""
        with mock.patch.dict(os.environ, {}, clear=True):
            legacy_file = self.mock_home / ".quickstream_bookmarks.json"
            sample_bookmarks = ["/Music/Rock", "/Music/Jazz"]
            legacy_file.write_text(json.dumps(sample_bookmarks), encoding="utf-8")

            target_file = get_bookmarks_path(migrate=True)
            expected_target = self.mock_home / ".config" / "quickstream" / "bookmarks.json"

            self.assertEqual(target_file, expected_target)
            self.assertTrue(expected_target.exists())
            self.assertEqual(json.loads(expected_target.read_text(encoding="utf-8")), sample_bookmarks)
            self.assertTrue(legacy_file.exists())

    def test_no_migration_when_target_already_exists(self):
        """Verify existing target config is not overwritten by legacy file."""
        with mock.patch.dict(os.environ, {}, clear=True):
            config_dir = self.mock_home / ".config" / "quickstream"
            config_dir.mkdir(parents=True, exist_ok=True)
            target_file = config_dir / "config.json"
            target_file.write_text(json.dumps({"metadata_view": True}), encoding="utf-8")

            legacy_file = self.mock_home / ".quickstream_config.json"
            legacy_file.write_text(json.dumps({"metadata_view": False}), encoding="utf-8")

            resolved = get_config_path(migrate=True)
            self.assertEqual(resolved, target_file)
            self.assertEqual(json.loads(target_file.read_text(encoding="utf-8")), {"metadata_view": True})

    def test_no_migration_when_legacy_does_not_exist(self):
        """Verify resolving path does not create any file when legacy does not exist."""
        with mock.patch.dict(os.environ, {}, clear=True):
            target = get_config_path(migrate=True)
            self.assertFalse(target.exists())
            self.assertEqual(target, self.mock_home / ".config" / "quickstream" / "config.json")

    def test_migration_failure_gracefully_falls_back_to_legacy(self):
        """Verify failure during file copying falls back to legacy path without crashing."""
        legacy = self.mock_home / "legacy.json"
        legacy.write_text("{}", encoding="utf-8")
        target = self.mock_home / "target" / "target.json"

        with mock.patch("shutil.copy2", side_effect=OSError("Disk full")):
            result = _migrate_file(target, legacy, label="test")
            self.assertEqual(result, legacy)


if __name__ == "__main__":
    unittest.main()
