//! Crash-safe persistence for Quickstream's own config and cache files (never library files).
//!
//! - [`atomic_write`] replaces a file by writing `<file>.<pid>.tmp` in the same directory,
//!   fsyncing it and renaming it over the target, so a crash leaves either the old or the new
//!   content, never a truncated file.
//! - [`load_json_or_backup`] copies a file that fails to parse to `<file>.bak` before the caller
//!   falls back to defaults, so the next save can't silently destroy the user's settings.

use serde::de::DeserializeOwned;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Temp file used while replacing `path`: same directory (so the rename stays on one
/// filesystem), suffixed with the process id so two running instances never share one.
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Replace `path` with `bytes` atomically: write a temp file beside it, fsync, rename.
/// On error the original file is left untouched.
///
/// Callers writing the same path from several threads must serialize those calls (the temp
/// path is per process, not per call).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        if tmp.is_file() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
    // Persist the rename itself. Best effort: not every filesystem supports fsync on a directory.
    if let Some(parent) = parent {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Keep a copy of a file that failed to parse and return where it went.
///
/// The first copy goes to `<file>.bak`. An existing backup is never overwritten: it only ever
/// holds content we refused to load, which may be the user's only copy of a hand-edited
/// config, so a later corrupt file goes to `<file>.bak.<timestamp>` instead. If an existing
/// backup already holds exactly these bytes (the same broken file loaded on every launch)
/// no new copy is made.
pub fn backup_corrupt_file(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let bak_prefix = format!("{}.bak", name);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == bak_prefix || fname.starts_with(&format!("{}.", bak_prefix)) {
                if fname.ends_with(".tmp") {
                    continue;
                }
                if fs::read(entry.path()).map(|b| b == bytes).unwrap_or(false) {
                    return Ok(entry.path());
                }
            }
        }
    }

    let mut target = dir.join(&bak_prefix);
    if target.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        target = dir.join(format!("{}.{}", bak_prefix, stamp));
        let mut n = 1;
        while target.exists() {
            target = dir.join(format!("{}.{}-{}", bak_prefix, stamp, n));
            n += 1;
        }
    }
    atomic_write(&target, bytes)?;
    Ok(target)
}

/// Read and parse the JSON file at `path`.
///
/// Returns `None` when the file is missing, unreadable, empty or fails to parse. A file that
/// fails to parse is first copied aside with [`backup_corrupt_file`] and the problem is logged,
/// so falling back to defaults (and saving them later) never loses the user's data. `what`
/// names the file in the log ("settings", "bookmarks").
pub fn load_json_or_backup<T: DeserializeOwned>(path: &Path, what: &str) -> Option<T> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            crate::logger::log_raw("WARN", "persist", &format!("could not read {} file '{}': {}", what, path.display(), e));
            return None;
        }
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        crate::logger::log_raw("WARN", "persist", &format!("{} file '{}' is empty; using defaults", what, path.display()));
        return None;
    }
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            let saved = match backup_corrupt_file(path, &bytes) {
                Ok(bak) => format!("copied to '{}'", bak.display()),
                Err(be) => format!("backup FAILED: {}", be),
            };
            crate::logger::log_raw(
                "WARN",
                "persist",
                &format!("{} file '{}' failed to parse ({}); {}; using defaults", what, path.display(), e, saved),
            );
            None
        }
    }
}
