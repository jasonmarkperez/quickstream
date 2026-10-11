"""Settings management and configuration persistence for Quickstream."""

from dataclasses import asdict, dataclass, field
import json
import logging
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger("quickstream.settings")

CONFIG_FILE_PATH = Path.home() / ".quickstream_config.json"


@dataclass
class Settings:
    """Quickstream user configuration options."""

    metadata_view: bool = True
    default_device: str = "local-audio"  # "local-audio", "last-connected"
    seek_step_seconds: float = 10.0      # 5.0, 10.0, 15.0, 30.0
    volume_step: int = 5                 # 2, 5, 10
    queue_mode: str = "loop"             # "loop", "stop", "repeat_one"
    force_transcode: bool = False        # False = Direct/Auto, True = PCM WAV


@dataclass
class SettingDefinition:
    """Metadata describing a single configurable option."""

    key: str
    label: str
    description: str
    options: List[Any]
    option_labels: Dict[Any, str]


SETTING_DEFINITIONS: List[SettingDefinition] = [
    SettingDefinition(
        key="metadata_view",
        label="File List Display",
        description="Display extracted ID3/FLAC tag metadata (Artist - Title) instead of filenames.",
        options=[True, False],
        option_labels={
            True: "Metadata (Tags)",
            False: "Filename (file.ext)",
        },
    ),
    SettingDefinition(
        key="default_device",
        label="Default Playback Device",
        description="Initial audio output target when starting Quickstream.",
        options=["local-audio", "last-connected"],
        option_labels={
            "local-audio": "Local Audio (ffplay)",
            "last-connected": "Last Connected Stereo",
        },
    ),
    SettingDefinition(
        key="seek_step_seconds",
        label="Seek Jump Interval",
        description="Amount of time to jump forward/backward when using Shift+Arrow keys.",
        options=[5.0, 10.0, 15.0, 30.0],
        option_labels={
            5.0: "5 seconds",
            10.0: "10 seconds",
            15.0: "15 seconds",
            30.0: "30 seconds",
        },
    ),
    SettingDefinition(
        key="volume_step",
        label="Volume Step Size",
        description="Percentage to increase or decrease volume per hotkey press (+/-).",
        options=[2, 5, 10],
        option_labels={
            2: "2%",
            5: "5%",
            10: "10%",
        },
    ),
    SettingDefinition(
        key="queue_mode",
        label="Queue Playback Mode",
        description="Playback behavior when reaching the end of the queue.",
        options=["loop", "stop", "repeat_one"],
        option_labels={
            "loop": "Continuous (Loop Queue)",
            "stop": "Stop at End",
            "repeat_one": "Repeat Current Track",
        },
    ),
    SettingDefinition(
        key="force_transcode",
        label="Audio Transcoding",
        description="Transcode all audio into 16-bit PCM WAV via ffmpeg for legacy DLNA renderers.",
        options=[False, True],
        option_labels={
            False: "Direct Stream (Auto)",
            True: "Force 16-bit PCM WAV",
        },
    ),
]


class SettingsManager:
    """Loads, saves, and mutates user settings with persistence."""

    def __init__(self, config_path: Optional[Path] = None):
        self.config_path = config_path or CONFIG_FILE_PATH
        self.settings: Settings = self.load()
        self.definitions = SETTING_DEFINITIONS

    def load(self) -> Settings:
        """Load settings from JSON file or return defaults if absent or invalid."""
        if not self.config_path.exists():
            return Settings()

        try:
            with open(self.config_path, "r", encoding="utf-8") as f:
                data = json.load(f)
            if not isinstance(data, dict):
                return Settings()

            # Filter valid fields in dataclass
            valid_keys = {f for f in Settings.__dataclass_fields__}
            filtered = {k: v for k, v in data.items() if k in valid_keys}
            return Settings(**filtered)
        except Exception as e:
            logger.warning("Error loading config from %s: %s; using defaults", self.config_path, e)
            return Settings()

    def save(self) -> bool:
        """Persist current settings to JSON file."""
        try:
            self.config_path.parent.mkdir(parents=True, exist_ok=True)
            with open(self.config_path, "w", encoding="utf-8") as f:
                json.dump(asdict(self.settings), f, indent=2)
            return True
        except Exception as e:
            logger.error("Failed to save config to %s: %s", self.config_path, e)
            return False

    def reset_defaults(self) -> Settings:
        """Reset settings to default values and save."""
        self.settings = Settings()
        self.save()
        return self.settings

    def get_value(self, key: str) -> Any:
        """Get current value of a setting."""
        return getattr(self.settings, key, None)

    def set_value(self, key: str, value: Any) -> bool:
        """Set a setting value and persist to disk."""
        if hasattr(self.settings, key):
            setattr(self.settings, key, value)
            return self.save()
        return False

    def cycle_setting(self, index: int, forward: bool = True) -> Any:
        """Cycle the value of the setting at the given definition index."""
        if 0 <= index < len(self.definitions):
            defn = self.definitions[index]
            current_val = getattr(self.settings, defn.key)
            if current_val in defn.options:
                idx = defn.options.index(current_val)
                step = 1 if forward else -1
                new_idx = (idx + step) % len(defn.options)
            else:
                new_idx = 0
            new_val = defn.options[new_idx]
            self.set_value(defn.key, new_val)
            return new_val
        return None

    def get_display_value(self, index: int) -> str:
        """Get formatted human-readable label for a setting."""
        if 0 <= index < len(self.definitions):
            defn = self.definitions[index]
            val = getattr(self.settings, defn.key)
            return defn.option_labels.get(val, str(val))
        return ""
