//! XDG Base Directory specification compliance and legacy file migration.

use std::fs;
use std::path::{Path, PathBuf};

/// Get the Quickstream configuration directory (~/.config/quickstream or $XDG_CONFIG_HOME/quickstream).
pub fn get_config_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim()).join("quickstream");
        }
    }
    if let Some(proj) = directories::BaseDirs::new() {
        return proj.config_dir().join("quickstream");
    }
    PathBuf::from(".config").join("quickstream")
}

/// Get the Quickstream cache directory (~/.cache/quickstream or $XDG_CACHE_HOME/quickstream).
pub fn get_cache_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_CACHE_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim()).join("quickstream");
        }
    }
    if let Some(proj) = directories::BaseDirs::new() {
        return proj.cache_dir().join("quickstream");
    }
    PathBuf::from(".cache").join("quickstream")
}

/// Get the Quickstream data directory (~/.local/share/quickstream or $XDG_DATA_HOME/quickstream).
pub fn get_data_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_DATA_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim()).join("quickstream");
        }
    }
    if let Some(proj) = directories::BaseDirs::new() {
        return proj.data_local_dir().join("quickstream");
    }
    PathBuf::from(".local").join("share").join("quickstream")
}

/// Migrate a legacy dotfile to the new target path if target does not already exist.
fn migrate_file(target: &Path, legacy: &Path) -> PathBuf {
    if target.exists() {
        return target.to_path_buf();
    }
    if legacy.exists() {
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::copy(legacy, target).is_ok() {
            return target.to_path_buf();
        }
        // Fallback to legacy path if copy fails
        return legacy.to_path_buf();
    }
    target.to_path_buf()
}

/// Get path to configuration file (config.json) with auto-migration.
pub fn get_config_path() -> PathBuf {
    let target = get_config_dir().join("config.json");
    if let Some(base) = directories::BaseDirs::new() {
        let legacy = base.home_dir().join(".quickstream_config.json");
        return migrate_file(&target, &legacy);
    }
    target
}

/// Get path to bookmarks file (bookmarks.json) with auto-migration.
pub fn get_bookmarks_path() -> PathBuf {
    let target = get_config_dir().join("bookmarks.json");
    if let Some(base) = directories::BaseDirs::new() {
        let legacy = base.home_dir().join(".quickstream_bookmarks.json");
        return migrate_file(&target, &legacy);
    }
    target
}

/// Get path to the library index cache file (music_index.jsonl).
pub fn get_music_index_path() -> PathBuf {
    get_cache_dir().join("music_index.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_resolution() {
        let dir = get_config_dir();
        assert!(dir.ends_with("quickstream"));
    }

    #[test]
    fn test_cache_dir_resolution() {
        let dir = get_cache_dir();
        assert!(dir.ends_with("quickstream"));
    }
}
