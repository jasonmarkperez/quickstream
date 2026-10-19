//! Legacy config migration (issue #60): legacy files (`~/.quickstream_config.json`,
//! `~/.quickstream_bookmarks.json`, macOS `~/Library/Application Support/quickstream`) are copied
//! only into the default `~/.config/quickstream`, never into a folder `XDG_CONFIG_HOME` names, and
//! a path to a legacy file is never handed out for writing.
//!
//! Every test points `HOME` at its own temp folder first and checks that it took effect, so no
//! test reads or writes the real home. The tests change process-wide variables, so they run one
//! at a time under `ENV_LOCK`, and `EnvGuard` restores `HOME` and `XDG_CONFIG_HOME` afterwards.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores `HOME` and `XDG_CONFIG_HOME` when dropped, also when the test panics.
struct EnvGuard {
    home: Option<OsString>,
    xdg_config: Option<OsString>,
}

impl EnvGuard {
    fn new() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            xdg_config: std::env::var_os("XDG_CONFIG_HOME"),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        restore("HOME", &self.home);
        restore("XDG_CONFIG_HOME", &self.xdg_config);
    }
}

fn restore(var: &str, value: &Option<OsString>) {
    match value {
        Some(v) => std::env::set_var(var, v),
        None => std::env::remove_var(var),
    }
}

/// A fresh, empty folder under the isolated root.
fn fresh_dir(name: &str) -> PathBuf {
    let dir = common::isolate().join("migration").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Point `HOME` at `home` and check that the path code sees it, before any app path is resolved.
fn set_home(home: &Path) {
    std::env::set_var("HOME", home);
    assert_eq!(std::env::var_os("HOME").map(PathBuf::from).as_deref(), Some(home));
    let base = directories::BaseDirs::new().expect("home folder");
    assert_eq!(base.home_dir(), home, "HOME did not take effect");
}

/// Fake legacy files in `home`: the dotfiles and, on macOS, the Application Support folder.
/// Returns every legacy file written.
fn write_legacy_files(home: &Path) -> Vec<PathBuf> {
    let mut written = vec![
        home.join(".quickstream_config.json"),
        home.join(".quickstream_bookmarks.json"),
    ];
    if cfg!(target_os = "macos") {
        let app_support = home.join("Library").join("Application Support").join("quickstream");
        std::fs::create_dir_all(&app_support).unwrap();
        written.push(app_support.join("config.json"));
        written.push(app_support.join("bookmarks.json"));
    }
    for file in &written {
        let body = if file.to_string_lossy().contains("bookmarks") {
            "[\"/legacy/music\"]"
        } else {
            "{}"
        };
        std::fs::write(file, body).unwrap();
    }
    written
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn test_no_legacy_migration_when_xdg_config_home_is_set() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("xdg-set-home");
    set_home(&home);
    let xdg = fresh_dir("xdg-set-config");
    std::env::set_var("XDG_CONFIG_HOME", &xdg);
    let legacy = write_legacy_files(&home);

    let config = quickstream::paths::get_config_path();
    let bookmarks = quickstream::paths::get_bookmarks_path();

    assert_eq!(config, xdg.join("quickstream").join("config.json"));
    assert_eq!(bookmarks, xdg.join("quickstream").join("bookmarks.json"));
    assert!(
        files_under(&xdg).is_empty(),
        "legacy files were copied into XDG_CONFIG_HOME: {:?}",
        files_under(&xdg)
    );
    assert!(!home.join(".config").exists(), "nothing is written to ~/.config either");
    for file in legacy {
        assert!(file.exists(), "{} was left in place", file.display());
    }
}

#[test]
fn test_legacy_files_migrate_into_default_config_dir() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("default-home");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    write_legacy_files(&home);
    let config_dir = home.join(".config").join("quickstream");
    assert_eq!(quickstream::paths::get_config_dir(), config_dir);

    let config = quickstream::paths::get_config_path();
    let bookmarks = quickstream::paths::get_bookmarks_path();

    assert_eq!(config, config_dir.join("config.json"));
    assert_eq!(bookmarks, config_dir.join("bookmarks.json"));
    assert!(config.is_file(), "config.json was migrated");
    assert_eq!(std::fs::read_to_string(&bookmarks).unwrap(), "[\"/legacy/music\"]");
}

#[test]
fn test_failed_migration_never_returns_a_legacy_path() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("copy-fails-home");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    let legacy = write_legacy_files(&home);
    // A file where ~/.config should be: the config folder can't be created, so the copy fails.
    std::fs::write(home.join(".config"), "not a folder").unwrap();
    let config_dir = home.join(".config").join("quickstream");

    let config = quickstream::paths::get_config_path();
    let bookmarks = quickstream::paths::get_bookmarks_path();

    assert_eq!(config, config_dir.join("config.json"));
    assert_eq!(bookmarks, config_dir.join("bookmarks.json"));
    for file in legacy {
        assert_ne!(config, file);
        assert_ne!(bookmarks, file);
    }
}
