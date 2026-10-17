//! Logger behaviour: background writer thread, ordering, size-based rotation, non-blocking callers.
//! Every test uses its own `Logger` instance writing into a temp dir, never the user's real log.

use quickstream::logger::{Logger, LoggerConfig};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn temp_log(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_logger_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("quickstream.log")
}

fn rotated(path: &std::path::Path, n: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), n))
}

#[test]
fn test_lines_from_multiple_threads_are_all_written_in_per_thread_order() {
    let path = temp_log("order");
    let logger = std::sync::Arc::new(Logger::with_config(path.clone(), LoggerConfig::default()));

    let threads: Vec<_> = (0..4)
        .map(|t| {
            let logger = logger.clone();
            std::thread::spawn(move || {
                for i in 0..500 {
                    logger.log("INFO", "order", &format!("thread={} seq={}", t, i));
                }
            })
        })
        .collect();
    for handle in threads {
        handle.join().unwrap();
    }
    assert!(logger.flush(), "flush should be acknowledged by the writer thread");

    let content = fs::read_to_string(&path).unwrap();
    let mut next = [0usize; 4];
    for line in content.lines().filter(|l| l.contains("[order]")) {
        assert!(line.contains("[INFO ] [order] thread="), "unexpected line format: {}", line);
        let rest = line.split("thread=").nth(1).unwrap();
        let (t, seq) = rest.split_once(" seq=").unwrap();
        let (t, seq): (usize, usize) = (t.parse().unwrap(), seq.parse().unwrap());
        assert_eq!(seq, next[t], "thread {} lines out of order", t);
        next[t] += 1;
    }
    assert_eq!(next, [500; 4], "every line from every thread must be written");
    assert_eq!(logger.dropped_count(), 0);
}

#[test]
fn test_log_file_rotates_at_size_limit_and_keeps_configured_backups() {
    let path = temp_log("rotate");
    let config = LoggerConfig { max_bytes: 1024, max_backups: 2, ..LoggerConfig::default() };
    let logger = Logger::with_config(path.clone(), config);

    for i in 0..200 {
        logger.log("INFO", "rotate", &format!("line {:04} padding padding padding", i));
    }
    assert!(logger.flush());

    let active = fs::read_to_string(&path).unwrap();
    let first_backup = fs::read_to_string(rotated(&path, 1)).unwrap();
    let second_backup = fs::read_to_string(rotated(&path, 2)).unwrap();
    assert!(!rotated(&path, 3).exists(), "only max_backups rotated files may be kept");

    for (name, content) in [("active", &active), (".1", &first_backup), (".2", &second_backup)] {
        assert!(content.len() <= 1024, "{} file exceeds the size limit: {} bytes", name, content.len());
        assert!(!content.is_empty(), "{} file is empty", name);
    }

    assert!(active.contains("line 0199"), "newest line belongs in the active file");
    let last_of = |s: &str| -> usize {
        let line = s.lines().rfind(|l| l.contains("[rotate]")).unwrap();
        line.split("line ").nth(1).unwrap()[..4].parse().unwrap()
    };
    let first_of = |s: &str| -> usize {
        let line = s.lines().find(|l| l.contains("[rotate]")).unwrap();
        line.split("line ").nth(1).unwrap()[..4].parse().unwrap()
    };
    assert_eq!(last_of(&second_backup) + 1, first_of(&first_backup), ".2 must directly precede .1");
    assert_eq!(last_of(&first_backup) + 1, first_of(&active), ".1 must directly precede the active file");
}

#[test]
fn test_existing_oversized_log_is_rotated_before_appending() {
    let path = temp_log("preexisting");
    fs::write(&path, "x".repeat(2048)).unwrap();
    let logger = Logger::with_config(path.clone(), LoggerConfig { max_bytes: 1024, ..LoggerConfig::default() });

    logger.log("INFO", "pre", "fresh line");
    assert!(logger.flush());

    assert_eq!(fs::read_to_string(rotated(&path, 1)).unwrap().len(), 2048);
    let active = fs::read_to_string(&path).unwrap();
    assert!(active.contains("fresh line") && !active.contains("xxxxxxxxxx"));
}

#[test]
fn test_caller_does_not_block_when_writer_is_slow() {
    let path = temp_log("slow");
    let config = LoggerConfig {
        queue_capacity: 8,
        write_delay: Some(Duration::from_millis(50)),
        ..LoggerConfig::default()
    };
    let logger = Logger::with_config(path.clone(), config);

    let start = Instant::now();
    for i in 0..1000 {
        logger.log("INFO", "slow", &format!("burst {}", i));
    }
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(250), "logging 1000 lines blocked the caller for {:?}", elapsed);
    assert!(logger.dropped_count() > 0, "a full queue must drop (and count) lines instead of blocking");

    // Once the writer catches up it records how many lines were lost.
    logger.log("INFO", "slow", "after burst");
    assert!(logger.flush());
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("burst 0"), "lines queued before the queue filled are still written");
    assert!(content.contains("[logger] dropped"), "dropped-line notice missing:\n{}", content);
}

#[test]
fn test_drop_flushes_pending_lines() {
    let path = temp_log("drop");
    {
        let logger = Logger::with_config(
            path.clone(),
            LoggerConfig { write_delay: Some(Duration::from_millis(2)), ..LoggerConfig::default() },
        );
        for i in 0..50 {
            logger.log("WARN", "drop", &format!("pending {}", i));
        }
    }
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("[WARN ] [drop] pending 49"), "drop must flush queued lines:\n{}", content);
}

#[test]
fn test_error_lines_reach_disk_without_explicit_flush() {
    let path = temp_log("error");
    let logger = Logger::with_config(path.clone(), LoggerConfig::default());
    logger.log("ERROR", "err", "something broke");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if fs::read_to_string(&path).unwrap_or_default().contains("[ERROR] [err] something broke") {
            break;
        }
        assert!(Instant::now() < deadline, "ERROR line was not flushed to disk");
        std::thread::sleep(Duration::from_millis(5));
    }
}
