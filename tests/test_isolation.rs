//! Tests never touch the developer's real quickstream directories (issue #29).
//!
//! `common::isolate()` points the XDG dirs at a per-process temp root; these tests check that
//! every app path then resolves under it, and that every integration test calls it first.

mod common;

use std::path::{Path, PathBuf};

#[test]
fn test_app_paths_resolve_under_isolated_root() {
    let root = common::isolate();
    let paths: Vec<PathBuf> = vec![
        quickstream::paths::get_config_dir(),
        quickstream::paths::get_cache_dir(),
        quickstream::paths::get_data_dir(),
        quickstream::paths::get_config_path(),
        quickstream::paths::get_bookmarks_path(),
        quickstream::paths::get_web_token_path(),
        quickstream::paths::get_music_index_path(),
        quickstream::paths::get_log_path(),
    ];
    let real = common::real_user_dirs();
    for path in paths {
        assert!(
            path.starts_with(root),
            "{} is outside {}",
            path.display(),
            root.display()
        );
        assert!(
            !real.iter().any(|r| path.starts_with(r)),
            "{} is a real user path",
            path.display()
        );
    }
    for (var, sub) in common::XDG_DIRS {
        assert_eq!(
            std::env::var_os(var).map(PathBuf::from),
            Some(root.join(sub)),
            "{}",
            var
        );
    }
}

#[test]
fn test_isolated_config_starts_from_defaults_not_legacy_files() {
    common::isolate();
    // A developer's legacy config / bookmarks must not be migrated into the isolated dir.
    let mgr = quickstream::settings::SettingsManager::new();
    assert_eq!(mgr.settings, quickstream::settings::Settings::default());
    let bookmarks = std::fs::read_to_string(quickstream::paths::get_bookmarks_path()).unwrap();
    assert_eq!(bookmarks.trim(), "[]");
}

#[test]
fn test_isolated_root_is_stable_within_process() {
    let first = common::isolate();
    let second = common::isolate();
    assert_eq!(first, second);
    assert!(first.to_string_lossy().contains(&std::process::id().to_string()));
}

/// Names of `#[test]` functions in `source` whose first statement does not call `common::isolate()`.
fn tests_missing_isolate(source: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut missing = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[test]" {
            i += 1;
            continue;
        }
        // Skip further attributes and comments up to the `fn` line, then to the line opening the body.
        let mut j = i + 1;
        while j < lines.len() && !lines[j].trim_start().starts_with("fn ") {
            j += 1;
        }
        let name = lines.get(j).map(|l| l.trim().to_string()).unwrap_or_default();
        while j < lines.len() && !lines[j].trim_end().ends_with('{') {
            j += 1;
        }
        let first_statement = lines.get(j + 1).map(|l| l.trim()).unwrap_or("");
        if !first_statement.contains("common::isolate()") {
            missing.push(name);
        }
        i = j + 1;
    }
    missing
}

#[test]
fn test_every_integration_test_isolates_xdg_dirs() {
    common::isolate();
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut problems = Vec::new();
    let mut checked = 0;
    for entry in std::fs::read_dir(&tests_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).unwrap();
        if !source.contains("#[test]") {
            continue;
        }
        checked += 1;
        if !source.lines().any(|l| l.trim() == "mod common;") {
            problems.push(format!("{}: missing `mod common;`", file));
        }
        for name in tests_missing_isolate(&source) {
            problems.push(format!("{}: `{}` does not call `common::isolate()` first", file, name));
        }
    }
    assert!(
        checked > 30,
        "only found {} test files in {}",
        checked,
        tests_dir.display()
    );
    assert!(
        problems.is_empty(),
        "integration tests must isolate XDG dirs:\n{}",
        problems.join("\n")
    );
}

#[test]
fn test_isolate_check_detects_missing_call() {
    common::isolate();
    let source =
        "#[test]\nfn good() {\n    common::isolate();\n}\n\n#[test]\n#[ignore]\nfn bad() {\n    let x = 1;\n}\n";
    assert_eq!(tests_missing_isolate(source), vec!["fn bad() {".to_string()]);
}
