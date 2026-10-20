//! Shared helpers for the integration tests (issue #29).
//!
//! Every test binary declares `mod common;` and every test starts with `common::isolate();`, so
//! nothing a test does (settings, bookmarks, web token, library cache, analysis database, log)
//! reads or writes the developer's real `~/.config/quickstream`, `~/.cache/quickstream` or
//! `~/.local/share/quickstream`. `tests/test_isolation.rs` checks that every test follows this.

#![allow(dead_code)]

#[cfg(unix)]
pub mod fake_exec;
pub mod fake_player;
pub mod tui;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The XDG variables `quickstream::paths` reads, and the subdirectory of the isolated root each
/// one points at.
pub const XDG_DIRS: [(&str, &str); 3] = [
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_DATA_HOME", "data"),
];

/// Point `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_DATA_HOME` at a fresh temp root for this
/// test process and return that root.
///
/// The variables are set in-process on the first call, so child processes spawned by a test
/// inherit them too. Call it first in every test: the harness runs tests on their own threads
/// in no fixed order, so it must run before whichever test first resolves an app path.
///
/// Every call also checks that the app paths do not resolve to the real user directories, in
/// case a test changed or removed a variable.
pub fn isolate() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    let root = ROOT.get_or_init(|| {
        let binary = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "tests".to_string());
        let root = std::env::temp_dir().join(format!("qs-test-{}-{}", binary, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (var, sub) in XDG_DIRS {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("cannot create {}: {}", dir.display(), e));
            std::env::set_var(var, &dir);
        }
        // With no config.json / bookmarks.json in the config dir, `paths` would copy the user's
        // legacy files (~/Library/Application Support/quickstream, ~/.quickstream_*.json) in,
        // so tests would run with the developer's settings and bookmarks. Start from defaults.
        let config = root.join("config").join("quickstream");
        std::fs::create_dir_all(&config).unwrap();
        let defaults = serde_json::to_string_pretty(&quickstream::settings::Settings::default()).unwrap();
        std::fs::write(config.join("config.json"), defaults).unwrap();
        std::fs::write(config.join("bookmarks.json"), "[]").unwrap();
        for dir in app_dirs() {
            assert!(
                dir.starts_with(&root),
                "{} is outside the isolated root {}",
                dir.display(),
                root.display()
            );
        }
        root
    });
    assert_not_real_user_dirs();
    root
}

/// Poll the browser's directory scan until the folder being opened is shown and listed (at
/// most 5 s). Opening a folder returns at once and leaves its listing to the event loop's poll
/// (issue #101).
pub fn wait_for_dir_scan(app: &mut quickstream::ui::AppUI) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let busy = |app: &quickstream::ui::AppUI| app.browser_loading || app.opening_folder().is_some();
    while busy(app) && std::time::Instant::now() < deadline {
        app.poll_background_dir_scan();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    app.poll_background_dir_scan();
    assert!(!busy(app), "{} was not listed", app.browser_dir.display());
}

/// The config, cache and data directories `quickstream::paths` currently resolves.
pub fn app_dirs() -> [PathBuf; 3] {
    [
        quickstream::paths::get_config_dir(),
        quickstream::paths::get_cache_dir(),
        quickstream::paths::get_data_dir(),
    ]
}

/// The real per-user quickstream directories (the defaults when no XDG variable is set).
pub fn real_user_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    vec![
        home.join(".config").join("quickstream"),
        home.join(".cache").join("quickstream"),
        home.join(".local").join("share").join("quickstream"),
    ]
}

/// Panic if any app directory resolves into the real per-user quickstream directories.
pub fn assert_not_real_user_dirs() {
    let real = real_user_dirs();
    for dir in app_dirs() {
        assert!(
            !real.iter().any(|r| dir.starts_with(r)),
            "test would use the real user directory {}; call common::isolate() first",
            dir.display()
        );
    }
}
