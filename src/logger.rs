//! Non-blocking, size-rotated file logging and performance profiling for Quickstream.
//!
//! Callers (including the UI thread) only capture a timestamp, build one `String` and hand it to a
//! bounded channel with `try_send`. A single background writer thread owns the file: it formats the
//! timestamp, writes through a `BufWriter`, flushes whenever the queue drains or an `ERROR` line is
//! written, and rotates `quickstream.log` -> `quickstream.log.1` -> ... once the size limit is hit.
//!
//! The queue is bounded on purpose: an unbounded queue never blocks either, but if the writer stalls
//! (slow/full disk, network home directory) it grows without limit. When the queue is full or the
//! writer has died, the line is dropped and counted instead of blocking the caller; the writer
//! records how many lines were lost as soon as it catches up.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

static LOGGER: OnceLock<Logger> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// How long `flush()` (and the at-exit / drop flush) waits for the writer before giving up.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// Tunables for a [`Logger`]. `Default` is what the application uses.
#[derive(Debug, Clone)]
pub struct LoggerConfig {
    /// Rotate once the active file would grow past this many bytes.
    pub max_bytes: u64,
    /// Number of rotated files to keep (`.1` is the newest, `.N` the oldest).
    pub max_backups: usize,
    /// Lines buffered between callers and the writer before new lines are dropped.
    pub queue_capacity: usize,
    /// Test hook: artificial delay per written line to simulate a slow disk.
    #[doc(hidden)]
    pub write_delay: Option<Duration>,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            max_bytes: 5 * 1024 * 1024,
            max_backups: 3,
            queue_capacity: 8192,
            write_delay: None,
        }
    }
}

enum Msg {
    Line { at: SystemTime, text: String, urgent: bool },
    Flush(SyncSender<()>),
    Shutdown,
}

/// A log sink backed by one writer thread. The application uses a single global instance via
/// [`init`] / [`log_raw`]; tests can create their own with [`Logger::with_config`].
pub struct Logger {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
    writer: Option<JoinHandle<()>>,
}

impl Logger {
    /// Start a logger appending to `path` with the default configuration.
    pub fn new(path: PathBuf) -> Self {
        Self::with_config(path, LoggerConfig::default())
    }

    /// Start a logger appending to `path`, spawning its writer thread.
    pub fn with_config(path: PathBuf, config: LoggerConfig) -> Self {
        let (tx, rx) = mpsc::sync_channel(config.queue_capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = dropped.clone();
        let writer = std::thread::Builder::new()
            .name("quickstream-logger".into())
            .spawn(move || run_writer(path, config, rx, writer_dropped))
            .ok();
        Self { tx, dropped, writer }
    }

    /// Queue a log line. Never blocks: if the queue is full or the writer is gone, the line is
    /// dropped and counted.
    pub fn log(&self, level: &str, module: &str, message: &str) {
        let msg = Msg::Line {
            at: SystemTime::now(),
            text: format!("[{:5}] [{}] {}\n", level, module, message),
            urgent: level == "ERROR",
        };
        if self.tx.try_send(msg).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Block (up to 2s) until every line queued before this call is written and flushed to disk.
    /// Returns `false` if the writer did not confirm in time.
    pub fn flush(&self) -> bool {
        self.flush_within(FLUSH_TIMEOUT)
    }

    fn flush_within(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if !self.send_control(Msg::Flush(ack_tx), deadline) {
            return false;
        }
        ack_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok()
    }

    /// Send a control message, retrying while the queue is full until `deadline`.
    fn send_control(&self, mut msg: Msg, deadline: Instant) -> bool {
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    msg = returned;
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// Total number of lines dropped because the queue was full or the writer had stopped.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        let sent = self.send_control(Msg::Shutdown, Instant::now() + FLUSH_TIMEOUT);
        if let (true, Some(handle)) = (sent, self.writer.take()) {
            let _ = handle.join();
        }
    }
}

struct LogFile {
    path: PathBuf,
    config: LoggerConfig,
    out: Option<BufWriter<File>>,
    size: u64,
}

impl LogFile {
    fn open(path: PathBuf, config: LoggerConfig) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut file = Self { path, config, out: None, size: 0 };
        file.reopen();
        file
    }

    fn reopen(&mut self) {
        self.out = None;
        self.size = 0;
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.out = Some(BufWriter::new(f));
        }
    }

    fn write_line(&mut self, line: &str) {
        let len = line.len() as u64;
        if self.size > 0 && self.size + len > self.config.max_bytes {
            self.rotate();
        }
        if let Some(out) = self.out.as_mut() {
            if out.write_all(line.as_bytes()).is_ok() {
                self.size += len;
            }
        }
    }

    fn rotate(&mut self) {
        self.flush();
        self.out = None;
        let backups = self.config.max_backups;
        if backups == 0 {
            let _ = fs::remove_file(&self.path);
        } else {
            let _ = fs::remove_file(numbered(&self.path, backups));
            for n in (1..backups).rev() {
                let _ = fs::rename(numbered(&self.path, n), numbered(&self.path, n + 1));
            }
            let _ = fs::rename(&self.path, numbered(&self.path, 1));
        }
        self.reopen();
    }

    fn flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            let _ = out.flush();
        }
    }
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}", n));
    PathBuf::from(name)
}

fn format_line(at: SystemTime, text: &str) -> String {
    let ts = chrono::DateTime::<chrono::Local>::from(at).format("%Y-%m-%d %H:%M:%S%.3f");
    format!("{} {}", ts, text)
}

fn run_writer(path: PathBuf, config: LoggerConfig, rx: Receiver<Msg>, dropped: Arc<AtomicU64>) {
    let delay = config.write_delay;
    let mut file = LogFile::open(path, config);
    let mut reported_drops = 0u64;

    // Block for the next message, then drain whatever else is queued before flushing once.
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(msg) = next.take() {
            match msg {
                Msg::Line { at, text, urgent } => {
                    let total_drops = dropped.load(Ordering::Relaxed);
                    if total_drops > reported_drops {
                        let notice = format!(
                            "[WARN ] [logger] dropped {} log lines (queue full or writer stalled)\n",
                            total_drops - reported_drops
                        );
                        file.write_line(&format_line(SystemTime::now(), &notice));
                        reported_drops = total_drops;
                    }
                    if let Some(delay) = delay {
                        std::thread::sleep(delay);
                    }
                    file.write_line(&format_line(at, &text));
                    if urgent {
                        file.flush();
                    }
                }
                Msg::Flush(ack) => {
                    file.flush();
                    let _ = ack.try_send(());
                }
                Msg::Shutdown => {
                    file.flush();
                    return;
                }
            }
            next = rx.try_recv().ok();
        }
        file.flush();
    }
    file.flush();
}

/// Initialize application logging to `~/.local/share/quickstream/quickstream.log`.
pub fn init() -> PathBuf {
    init_at(crate::paths::get_log_path())
}

/// Initialize the global logger at an explicit path. Only the first call takes effect.
pub(crate) fn init_at(log_path: PathBuf) -> PathBuf {
    let _ = LOG_PATH.set(log_path.clone());
    let mut started = false;
    LOGGER.get_or_init(|| {
        started = true;
        Logger::new(log_path.clone())
    });
    if started {
        // Statics are never dropped, so flush queued lines when the process exits normally.
        // SAFETY: registering a plain `extern "C" fn` with no captured state.
        unsafe {
            libc::atexit(flush_at_exit);
        }
    }

    let active = get_log_file_path();
    log_raw("INFO", "logger", &format!("Logging initialized at {}", active.display()));
    active
}

extern "C" fn flush_at_exit() {
    let _ = std::panic::catch_unwind(|| {
        if let Some(logger) = LOGGER.get() {
            logger.flush_within(EXIT_FLUSH_TIMEOUT);
        }
    });
}

/// Get the path of the primary log file.
pub fn get_log_file_path() -> PathBuf {
    LOG_PATH.get().cloned().unwrap_or_else(crate::paths::get_log_path)
}

/// Write a structured log message with timestamp, level, and module tag. Never blocks on I/O.
pub fn log_raw(level: &str, module: &str, message: &str) {
    if let Some(logger) = LOGGER.get() {
        logger.log(level, module, message);
    }
}

/// Wait (up to 2s) until everything logged so far through the global logger is on disk.
/// Returns `false` if logging is not initialized or the writer did not respond in time.
pub fn flush() -> bool {
    LOGGER.get().is_some_and(Logger::flush)
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

    /// The global logger for this test binary always lives in a temp dir, never the user's real log.
    fn init_temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qs_logger_unit_{}", std::process::id()));
        init_at(dir.join("quickstream.log"))
    }

    #[test]
    fn test_logger_initialization_and_logging() {
        let path = init_temp();
        log_raw("INFO", "test", "Hello test log");
        let timer = PerfTimer::new("test", "test_operation");
        std::thread::sleep(std::time::Duration::from_millis(5));
        drop(timer);
        assert!(flush());

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("[INFO ] [test] Hello test log"));
        assert!(content.contains("[PERF ] [test] test_operation -> elapsed:"));
    }

    #[test]
    fn test_logger_does_not_write_to_working_directory() {
        init_temp();
        let marker = format!("cwd-log-marker-{}", std::process::id());
        log_raw("INFO", "test", &marker);
        assert!(flush());

        let cwd_log = std::env::current_dir().unwrap().join("quickstream.log");
        let content = std::fs::read_to_string(&cwd_log).unwrap_or_default();
        assert!(!content.contains(&marker), "logger wrote to {}", cwd_log.display());
    }
}
