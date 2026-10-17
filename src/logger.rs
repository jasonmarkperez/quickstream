//! High-performance, zero-external-dependency logging and performance profiling framework for Quickstream.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Initialize application logging to `~/.local/share/quickstream/quickstream.log`.
pub fn init() -> PathBuf {
    let log_path = crate::paths::get_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
    let _ = LOG_PATH.set(log_path.clone());

    log_raw("INFO", "logger", &format!("Logging initialized at {}", log_path.display()));
    log_path
}

/// Get the path of the primary log file.
pub fn get_log_file_path() -> PathBuf {
    LOG_PATH.get().cloned().unwrap_or_else(crate::paths::get_log_path)
}

/// Write a structured log message with timestamp, level, and module tag.
pub fn log_raw(level: &str, module: &str, message: &str) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("{} [{:5}] [{}] {}\n", now, level, module, message);

    if let Some(mutex) = LOG_FILE.get() {
        if let Ok(mut file) = mutex.lock() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

/// A lightweight RAII performance scope timer that automatically logs execution time when dropped or finished.
pub struct PerfTimer {
    module: &'static str,
    action: String,
    start: Instant,
    recorded: bool,
}

impl PerfTimer {
    pub fn new(module: &'static str, action: impl Into<String>) -> Self {
        Self {
            module,
            action: action.into(),
            start: Instant::now(),
            recorded: false,
        }
    }

    pub fn finish(mut self) {
        self.record();
    }

    fn record(&mut self) {
        if !self.recorded {
            self.recorded = true;
            let elapsed = self.start.elapsed();
            log_raw(
                "PERF",
                self.module,
                &format!("{} -> elapsed: {:.2?}", self.action, elapsed),
            );
        }
    }
}

impl Drop for PerfTimer {
    fn drop(&mut self) {
        self.record();
    }
}

#[macro_export]
macro_rules! log_info {
    ($module:expr, $($arg:tt)*) => {
        $crate::logger::log_raw("INFO", $module, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($module:expr, $($arg:tt)*) => {
        $crate::logger::log_raw("WARN", $module, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($module:expr, $($arg:tt)*) => {
        $crate::logger::log_raw("ERROR", $module, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_perf {
    ($module:expr, $($arg:tt)*) => {
        $crate::logger::PerfTimer::new($module, format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logger_initialization_and_logging() {
        init();
        log_raw("INFO", "test", "Hello test log");
        let timer = PerfTimer::new("test", "test_operation");
        std::thread::sleep(std::time::Duration::from_millis(5));
        drop(timer);

        let path = get_log_file_path();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("[INFO ] [test] Hello test log"));
        assert!(content.contains("[PERF ] [test] test_operation -> elapsed:"));
    }

    #[test]
    fn test_logger_does_not_write_to_working_directory() {
        init();
        let marker = format!("cwd-log-marker-{}", std::process::id());
        log_raw("INFO", "test", &marker);

        let cwd_log = std::env::current_dir().unwrap().join("quickstream.log");
        let content = std::fs::read_to_string(&cwd_log).unwrap_or_default();
        assert!(!content.contains(&marker), "logger wrote to {}", cwd_log.display());
    }
}
