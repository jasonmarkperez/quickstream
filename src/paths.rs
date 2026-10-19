//! XDG Base Directory specification compliance and legacy file migration.

use std::fs;
use std::path::{Path, PathBuf};

/// `path` with the home folder shown as `~`, in its original case (Telemetry resolution 2).
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Get the Quickstream configuration directory (~/.config/quickstream or $XDG_CONFIG_HOME/quickstream).
pub fn get_config_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim()).join("quickstream");
        }
    }
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".config").join("quickstream");
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
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".cache").join("quickstream");
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
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".local").join("share").join("quickstream");
    }
    PathBuf::from(".local").join("share").join("quickstream")
}

/// Migrate a legacy file to the new target path if target does not already exist.
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
    if target.exists() {
        return target;
    }
    if let Some(base) = directories::BaseDirs::new() {
        // First check macOS ~/Library/Application Support/quickstream/config.json
        let app_support = base.config_dir().join("quickstream").join("config.json");
        if app_support.exists() {
            return migrate_file(&target, &app_support);
        }
        // Then check legacy ~/.quickstream_config.json
        let legacy = base.home_dir().join(".quickstream_config.json");
        return migrate_file(&target, &legacy);
    }
    target
}

/// Get path to bookmarks file (bookmarks.json) with auto-migration.
pub fn get_bookmarks_path() -> PathBuf {
    let target = get_config_dir().join("bookmarks.json");
    if target.exists() {
        return target;
    }
    if let Some(base) = directories::BaseDirs::new() {
        // First check macOS ~/Library/Application Support/quickstream/bookmarks.json
        let app_support = base.config_dir().join("quickstream").join("bookmarks.json");
        if app_support.exists() {
            return migrate_file(&target, &app_support);
        }
        // Then check legacy ~/.quickstream_bookmarks.json
        let legacy = base.home_dir().join(".quickstream_bookmarks.json");
        return migrate_file(&target, &legacy);
    }
    target
}

/// Get the playlists folder (playlists/, one `<name>.m3u8` per playlist). Not created here: the
/// background writer creates it with the first playlist it saves.
pub fn get_playlists_dir() -> PathBuf {
    get_config_dir().join("playlists")
}

/// Get path to the web dashboard session token (web_token, mode 0600).
pub fn get_web_token_path() -> PathBuf {
    get_config_dir().join("web_token")
}

/// Get path to the library index cache file (music_index.jsonl).
pub fn get_music_index_path() -> PathBuf {
    get_cache_dir().join("music_index.jsonl")
}

/// Get path to the application log file (quickstream.log).
pub fn get_log_path() -> PathBuf {
    get_data_dir().join("quickstream.log")
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

    #[test]
    fn test_log_path_resolution() {
        let path = get_log_path();
        assert!(path.ends_with("quickstream.log"));
        assert_eq!(path.parent(), Some(get_data_dir().as_path()));
    }
}
