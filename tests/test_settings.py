"""Automated unit tests for Settings, SettingsManager, and persistence."""

from pathlib import Path
import tempfile
import unittest

from quickstream.settings import Settings, SettingsManager


class TestSettings(unittest.TestCase):
    """Test Settings dataclass, serialization, option cycling, and resilience."""

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.config_path = Path(self.temp_dir.name) / "config.json"
        self.manager = SettingsManager(config_path=self.config_path)

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_default_settings(self):
        """Verify default settings values."""
        s = self.manager.settings
        self.assertTrue(s.metadata_view)
        self.assertEqual(s.default_device, "local-audio")
        self.assertEqual(s.seek_step_seconds, 10.0)
        self.assertEqual(s.volume_step, 5)
        self.assertEqual(s.queue_mode, "loop")
        self.assertFalse(s.force_transcode)

    def test_save_and_load_roundtrip(self):
        """Verify settings persist and reload correctly."""
        self.manager.settings.metadata_view = False
        self.manager.settings.seek_step_seconds = 30.0
        self.manager.settings.volume_step = 10
        self.manager.settings.queue_mode = "stop"
        self.manager.settings.force_transcode = True
        saved = self.manager.save()
        self.assertTrue(saved)
        self.assertTrue(self.config_path.exists())

        new_manager = SettingsManager(config_path=self.config_path)
        s = new_manager.settings
        self.assertFalse(s.metadata_view)
        self.assertEqual(s.seek_step_seconds, 30.0)
        self.assertEqual(s.volume_step, 10)
        self.assertEqual(s.queue_mode, "stop")
        self.assertTrue(s.force_transcode)

    def test_corrupt_config_recovers_gracefully(self):
        """Verify corrupt or non-dict JSON falls back to default settings."""
        with open(self.config_path, "w", encoding="utf-8") as f:
            f.write("NOT_A_VALID_JSON{:::}")

        recovered = SettingsManager(config_path=self.config_path)
        self.assertTrue(recovered.settings.metadata_view)
        self.assertEqual(recovered.settings.seek_step_seconds, 10.0)

    def test_cycle_setting_forward_and_backward(self):
        """Verify cycle_setting updates values and wraps around options."""
        # Index 0 is metadata_view ([True, False])
        val1 = self.manager.cycle_setting(0, forward=True)
        self.assertFalse(val1)
        self.assertFalse(self.manager.settings.metadata_view)

        val2 = self.manager.cycle_setting(0, forward=True)
        self.assertTrue(val2)
        self.assertTrue(self.manager.settings.metadata_view)

        val3 = self.manager.cycle_setting(0, forward=False)
        self.assertFalse(val3)
        self.assertFalse(self.manager.settings.metadata_view)

    def test_cycle_seek_step_options(self):
        """Verify cycling through seek steps (5s, 10s, 15s, 30s)."""
        # Find index for seek_step_seconds
        seek_idx = next(i for i, d in enumerate(self.manager.definitions) if d.key == "seek_step_seconds")
        self.assertEqual(self.manager.settings.seek_step_seconds, 10.0)

        self.manager.cycle_setting(seek_idx, forward=True)
        self.assertEqual(self.manager.settings.seek_step_seconds, 15.0)

        self.manager.cycle_setting(seek_idx, forward=True)
        self.assertEqual(self.manager.settings.seek_step_seconds, 30.0)

        self.manager.cycle_setting(seek_idx, forward=True)
        self.assertEqual(self.manager.settings.seek_step_seconds, 5.0)

    def test_get_display_value(self):
        """Verify formatted label display."""
        # Index 0 = metadata_view
        self.manager.settings.metadata_view = True
        self.assertEqual(self.manager.get_display_value(0), "Metadata (Tags)")

        self.manager.settings.metadata_view = False
        self.assertEqual(self.manager.get_display_value(0), "Filename (file.ext)")

    def test_reset_defaults(self):
        """Verify reset_defaults restores clean settings."""
        self.manager.settings.seek_step_seconds = 30.0
        self.manager.settings.metadata_view = False
        self.manager.save()

        self.manager.reset_defaults()
        self.assertEqual(self.manager.settings.seek_step_seconds, 10.0)
        self.assertTrue(self.manager.settings.metadata_view)


if __name__ == "__main__":
    unittest.main()
