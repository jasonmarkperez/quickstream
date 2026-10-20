//! XDG Base Directory specification compliance and legacy file migration.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        return default_config_dir(base.home_dir());
    }
    PathBuf::from(".config").join("quickstream")
}

/// The config folder when `XDG_CONFIG_HOME` is not set: `~/.config/quickstream`.
fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("quickstream")
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

/// A config file resolved once per process (#60): where it is read from at startup and where
/// every save goes. The two differ only when copying a legacy file into the config folder
/// failed: the app then loads the legacy file (read only) and saves to the new path, so the
/// first save carries the legacy content forward. A legacy file is never a `write_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub read_path: PathBuf,
    pub write_path: PathBuf,
}

impl ConfigFile {
    /// A file read from and saved to `path`.
    pub fn at(path: PathBuf) -> Self {
        Self {
            read_path: path.clone(),
            write_path: path,
        }
    }

    /// Load and parse the file: `read_path`, or `write_path` once a save has created it. A file
    /// that fails to parse is backed up beside `write_path`
    /// (`persist::load_json_or_backup_beside`), so nothing is ever written beside a legacy file.
    pub fn load_json<T: serde::de::DeserializeOwned>(&self, what: &str) -> Option<T> {
        let path = if self.read_path != self.write_path && self.write_path.exists() {
            &self.write_path
        } else {
            &self.read_path
        };
        crate::persist::load_json_or_backup_beside(path, &self.write_path, what)
    }
}

/// Copy `legacy` to `target`, which doesn't exist yet, never replacing a file there: the bytes
/// go to a temp file that is then hard-linked into place (`persist::create_new_file`), so a
/// failed copy (a full disk) leaves no half-written target. When the copy fails the legacy file
/// is read instead, and saves still go to `target`.
fn migrate_file(target: PathBuf, legacy: &Path) -> ConfigFile {
    let copied = fs::read(legacy).and_then(|bytes| crate::persist::create_new_file(&target, &bytes));
    match copied {
        Ok(()) => ConfigFile::at(target),
        // Another Quickstream migrated it first.
        Err(_) if target.exists() => ConfigFile::at(target),
        Err(e) => {
            crate::log_warn!(
                "paths",
                "Couldn't migrate {} to {}: {}; reading it from there and saving to {}",
                legacy.display(),
                target.display(),
                e,
                target.display()
            );
            ConfigFile {
                read_path: legacy.to_path_buf(),
                write_path: target,
            }
        }
    }
}

/// `path` with symlinks resolved: its longest existing ancestor canonicalized and the rest
/// appended as written, or `path` as written when not even that resolves.
fn resolved_path(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(canonical) = fs::canonicalize(current) {
            return rest.iter().rev().fold(canonical, |dir, name| dir.join(name));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `name` in the config folder, migrated from the first legacy file that exists: the macOS
/// `~/Library/Application Support/quickstream/<name>`, then `~/<dotfile>`. Legacy files are only
/// migrated into the default `~/.config/quickstream` (compared with symlinks resolved); a folder
/// `XDG_CONFIG_HOME` names (#60) starts from its own files.
fn resolve_config_file(dir: &Path, name: &str, dotfile: &str) -> ConfigFile {
    let target = dir.join(name);
    if target.exists() {
        return ConfigFile::at(target);
    }
    let Some(base) = directories::BaseDirs::new() else {
        return ConfigFile::at(target);
    };
    let legacy = [
        base.config_dir().join("quickstream").join(name),
        base.home_dir().join(dotfile),
    ]
    .into_iter()
    .find(|file| file.exists());
    let Some(legacy) = legacy else {
        return ConfigFile::at(target);
    };
    if resolved_path(dir) != resolved_path(&default_config_dir(base.home_dir())) {
        crate::log_info!(
            "paths",
            "Not migrating {} into {}: XDG_CONFIG_HOME names a folder other than ~/.config",
            legacy.display(),
            dir.display()
        );
        return ConfigFile::at(target);
    }
    migrate_file(target, &legacy)
}

/// `name` in the config folder, resolved (and migrated) the first time it is asked for and the
/// same answer every time after, so no later lookup touches the disk or retries a failed copy.
/// Keyed by the config folder, which only tests change while running.
fn config_file_named(name: &str, dotfile: &str) -> ConfigFile {
    static RESOLVED: Mutex<BTreeMap<PathBuf, ConfigFile>> = Mutex::new(BTreeMap::new());
    let dir = get_config_dir();
    // Held while resolving, so two threads never migrate the same file at once.
    let mut resolved = RESOLVED.lock().unwrap_or_else(|e| e.into_inner());
    resolved
        .entry(dir.join(name))
        .or_insert_with(|| resolve_config_file(&dir, name, dotfile))
        .clone()
}

/// The configuration file (config.json), migrated from a legacy file once per process.
pub fn config_file() -> ConfigFile {
    config_file_named("config.json", ".quickstream_config.json")
}

/// The bookmarks file (bookmarks.json), migrated from a legacy file once per process.
pub fn bookmarks_file() -> ConfigFile {
    config_file_named("bookmarks.json", ".quickstream_bookmarks.json")
}

/// Where the configuration file (config.json) is saved; [`config_file`] says where it is read.
pub fn get_config_path() -> PathBuf {
    config_file().write_path
}

/// Where the bookmarks file (bookmarks.json) is saved; [`bookmarks_file`] says where it is read.
pub fn get_bookmarks_path() -> PathBuf {
    bookmarks_file().write_path
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
