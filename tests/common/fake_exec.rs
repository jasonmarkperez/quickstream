//! Fake executables for tests (issue #80): stand-in `mpv`, `ffplay` or `ffmpeg` scripts a test
//! writes and then runs.

use std::path::Path;
use std::process::Command;

/// Write `text` to `path` as an executable script (mode 755), replacing any file there.
///
/// The script is written by a short-lived `/bin/sh`, never through a descriptor of this
/// process. On Linux exec fails with ETXTBSY while any process holds the file open for writing,
/// and a child another test thread forks while this process has it open (`fs::write`) keeps that
/// descriptor until the child's own exec, so the script could fail to start. The `sh` closes its
/// descriptor when it exits, which `status()` waits for, and children of this process never see
/// it. Writing to a temporary name and renaming it into place would not help: the inherited
/// descriptor is on the same file.
pub fn write_script(path: &Path, text: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"", "sh"])
        .arg(text)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "could not write {}", path.display());
}
