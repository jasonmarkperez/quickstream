"""Filesystem path resolution and XDG directory standard compliance for Quickstream."""

import logging
import os
from pathlib import Path
import shutil
from typing import Optional

logger = logging.getLogger("quickstream.paths")


def get_config_dir() -> Path:
    """
    Get the Quickstream configuration directory following XDG Base Directory specification.
    Defaults to ~/.config/quickstream unless $XDG_CONFIG_HOME is set.
    """
    xdg_config = os.environ.get("XDG_CONFIG_HOME")
    if xdg_config and xdg_config.strip():
        base = Path(xdg_config.strip())
    else:
        base = Path.home() / ".config"
    return base / "quickstream"


def get_cache_dir() -> Path:
    """
    Get the Quickstream cache directory following XDG Base Directory specification.
    Defaults to ~/.cache/quickstream unless $XDG_CACHE_HOME is set.
    """
    xdg_cache = os.environ.get("XDG_CACHE_HOME")
    if xdg_cache and xdg_cache.strip():
        base = Path(xdg_cache.strip())
    else:
        base = Path.home() / ".cache"
    return base / "quickstream"


def get_data_dir() -> Path:
    """
    Get the Quickstream data directory following XDG Base Directory specification.
    Defaults to ~/.local/share/quickstream unless $XDG_DATA_HOME is set.
    """
    xdg_data = os.environ.get("XDG_DATA_HOME")
    if xdg_data and xdg_data.strip():
        base = Path(xdg_data.strip())
    else:
        base = Path.home() / ".local" / "share"
    return base / "quickstream"


def _migrate_file(target_path: Path, legacy_path: Path, label: str = "file") -> Path:
    """
    Ensure target_path exists by migrating legacy_path if needed.
    Returns target_path if available or migrated; falls back to legacy_path if migration fails.
    """
    if target_path.exists():
        return target_path

    if legacy_path.exists():
        try:
            target_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(legacy_path, target_path)
            logger.info("Migrated legacy %s from %s to %s", label, legacy_path, target_path)
            return target_path
        except Exception as e:
            logger.warning(
                "Could not migrate legacy %s from %s to %s: %s; falling back to legacy path",
                label,
                legacy_path,
                target_path,
                e,
            )
            return legacy_path

    return target_path


def get_config_path(migrate: bool = True) -> Path:
    """
    Get path to the Quickstream configuration file (config.json).
    Automatically migrates ~/.quickstream_config.json if the target does not exist.
    """
    target = get_config_dir() / "config.json"
    if migrate:
        legacy = Path.home() / ".quickstream_config.json"
        return _migrate_file(target, legacy, label="configuration")
    return target


def get_bookmarks_path(migrate: bool = True) -> Path:
    """
    Get path to the Quickstream bookmarks file (bookmarks.json).
    Automatically migrates ~/.quickstream_bookmarks.json if the target does not exist.
    """
    target = get_config_dir() / "bookmarks.json"
    if migrate:
        legacy = Path.home() / ".quickstream_bookmarks.json"
        return _migrate_file(target, legacy, label="bookmarks")
    return target
