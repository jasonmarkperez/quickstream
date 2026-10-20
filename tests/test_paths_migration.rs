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

use crossterm::event::KeyCode;
use quickstream::settings::{Settings, SettingsManager};
use quickstream::ui::AppUI;

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
            LEGACY_BOOKMARKS.to_string()
        } else {
            serde_json::to_string_pretty(&legacy_settings()).unwrap()
        };
        std::fs::write(file, body).unwrap();
    }
    written
}

/// What the fake legacy bookmarks files hold.
const LEGACY_BOOKMARKS: &str = "[\"/legacy/music\"]";

/// What the fake legacy config files hold: the defaults with a volume step of 7.
fn legacy_settings() -> Settings {
    Settings {
        volume_step: 7,
        ..Settings::default()
    }
}

/// The bytes of every file in `files`, to check later that none was written.
fn contents(files: &[PathBuf]) -> Vec<Vec<u8>> {
    files.iter().map(|f| std::fs::read(f).unwrap()).collect()
}

/// A file where `~/.config` should be: the config folder can't be created, so copying a legacy
/// file into it (and saving there) fails until [`unblock_config`] removes it.
fn block_config(home: &Path) {
    std::fs::write(home.join(".config"), "not a folder").unwrap();
}

fn unblock_config(home: &Path) {
    std::fs::remove_file(home.join(".config")).unwrap();
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
    assert_eq!(std::fs::read_to_string(&bookmarks).unwrap(), LEGACY_BOOKMARKS);
    let mut migrated = files_under(&config_dir);
    migrated.sort();
    assert_eq!(migrated, vec![bookmarks, config], "no temp file is left behind");
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
    block_config(&home);
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

#[test]
fn test_after_a_failed_copy_settings_load_from_the_legacy_file_and_save_to_the_new_path() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("settings-forward-home");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    let legacy = write_legacy_files(&home);
    let before = contents(&legacy);
    block_config(&home);

    let mut mgr = SettingsManager::new();
    assert_eq!(mgr.settings, legacy_settings(), "the legacy settings are loaded");
    let config = home.join(".config").join("quickstream").join("config.json");
    assert_eq!(mgr.path, config, "saves go to the new path");

    // The folder becomes writable later in the session; the first save carries the legacy
    // settings forward with the change.
    unblock_config(&home);
    mgr.settings.seek_step_seconds = 30.0;
    assert!(mgr.save());

    let saved = SettingsManager::load_from(&config).expect("settings saved to the new path");
    assert_eq!(saved.volume_step, 7, "the legacy value was carried forward");
    assert_eq!(saved.seek_step_seconds, 30.0);
    assert_eq!(contents(&legacy), before, "legacy files are never written");
}

#[test]
fn test_after_a_failed_copy_bookmarks_load_from_the_legacy_file_and_save_to_the_new_path() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("bookmarks-forward-home");
    let settings_path = fresh_dir("bookmarks-forward-settings").join("config.json");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    let legacy = write_legacy_files(&home);
    let before = contents(&legacy);
    block_config(&home);
    let music = home.join("music");
    std::fs::create_dir_all(&music).unwrap();

    let mut settings_mgr = SettingsManager::with_path(settings_path);
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(music.clone()), settings_mgr);
    assert_eq!(
        app.bookmarks,
        vec!["/legacy/music".to_string()],
        "the legacy bookmarks are loaded"
    );

    unblock_config(&home);
    app.active_tab = 1;
    app.handle_tab_key(KeyCode::Char('b'));
    assert!(app.persistence.flush());

    let bookmarks = home.join(".config").join("quickstream").join("bookmarks.json");
    let saved: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&bookmarks).unwrap()).unwrap();
    let browsed = app.browser_dir.to_string_lossy().to_string();
    assert_eq!(
        saved,
        vec!["/legacy/music".to_string(), browsed],
        "legacy bookmarks + the new one"
    );
    assert_eq!(contents(&legacy), before, "legacy files are never written");
}

#[test]
fn test_a_lookup_after_a_failed_copy_does_not_copy_again() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("no-retry-home");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    write_legacy_files(&home);
    block_config(&home);
    let first = (quickstream::paths::config_file(), quickstream::paths::bookmarks_file());

    unblock_config(&home);
    let second = (quickstream::paths::config_file(), quickstream::paths::bookmarks_file());
    let _ = quickstream::paths::get_config_path();
    let _ = quickstream::paths::get_bookmarks_path();

    assert_eq!(first, second, "the path is resolved once");
    assert!(
        files_under(&home.join(".config")).is_empty(),
        "nothing was copied on a later lookup: {:?}",
        files_under(&home.join(".config"))
    );
}

#[test]
fn test_a_corrupt_legacy_file_is_never_backed_up_beside_itself() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("corrupt-legacy-home");
    set_home(&home);
    std::env::remove_var("XDG_CONFIG_HOME");
    let legacy = write_legacy_files(&home);
    for file in &legacy {
        std::fs::write(file, "{ not json").unwrap();
    }
    block_config(&home);
    let listed = |dir: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        names
    };
    let legacy_dirs: Vec<PathBuf> = legacy.iter().map(|f| f.parent().unwrap().to_path_buf()).collect();
    let before: Vec<_> = legacy_dirs.iter().map(|d| listed(d)).collect();

    let mgr = SettingsManager::new();

    assert_eq!(mgr.settings, Settings::default());
    let after: Vec<_> = legacy_dirs.iter().map(|d| listed(d)).collect();
    assert_eq!(after, before, "nothing is written beside a legacy file");
}

#[test]
fn test_xdg_config_home_naming_the_default_folder_through_a_symlink_still_migrates() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let home = fresh_dir("symlink-home");
    set_home(&home);
    write_legacy_files(&home);
    std::fs::create_dir_all(home.join(".config")).unwrap();
    let link = home.join("config-link");
    std::os::unix::fs::symlink(home.join(".config"), &link).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &link);

    let bookmarks = quickstream::paths::get_bookmarks_path();

    assert_eq!(bookmarks, link.join("quickstream").join("bookmarks.json"));
    let migrated = home.join(".config").join("quickstream").join("bookmarks.json");
    assert_eq!(std::fs::read_to_string(&migrated).unwrap(), LEGACY_BOOKMARKS);
}

#[test]
fn test_skipped_migration_is_logged() {
    common::isolate();
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new();
    let log_path = quickstream::logger::init();
    let home = fresh_dir("skip-log-home");
    set_home(&home);
    let xdg = fresh_dir("skip-log-config");
    std::env::set_var("XDG_CONFIG_HOME", &xdg);
    let legacy = write_legacy_files(&home);

    let _ = quickstream::paths::get_bookmarks_path();

    assert!(quickstream::logger::flush(), "log writer did not flush");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let names_legacy_bookmarks = |line: &str| {
        legacy
            .iter()
            .filter(|f| f.to_string_lossy().contains("bookmarks"))
            .any(|f| line.contains(&*f.to_string_lossy()))
    };
    assert!(
        log.lines()
            .any(|l| l.contains("[paths]") && l.contains("XDG_CONFIG_HOME") && names_legacy_bookmarks(l)),
        "no log line about the skipped migration in:\n{log}"
    );
}
