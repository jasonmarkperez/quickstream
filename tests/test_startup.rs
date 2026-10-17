//! Binary startup: version reporting (issue #25).

mod common;

use std::process::Command;

#[test]
fn test_startup_log_reports_crate_version() {
    common::isolate();
    let root = std::env::temp_dir().join(format!("qs_startup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // `--info` on a missing file exits right after logging is initialized.
    let output = Command::new(env!("CARGO_BIN_EXE_quickstream"))
        .arg("--info")
        .arg(root.join("missing.wav"))
        .current_dir(&root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .output()
        .unwrap();
    assert!(!output.status.success());

    let log = std::fs::read_to_string(root.join("data").join("quickstream").join("quickstream.log")).unwrap();
    let expected = format!("Starting Quickstream v{} ", env!("CARGO_PKG_VERSION"));
    assert!(log.contains(&expected), "log should contain {:?}:\n{}", expected, log);

    let _ = std::fs::remove_dir_all(&root);
}
