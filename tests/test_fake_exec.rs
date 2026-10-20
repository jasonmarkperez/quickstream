//! `common::fake_exec`: fake executables the tests write and then run (issue #80).
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

/// Fresh scratch dir unique to this process and test. (The XDG dirs are isolated by
/// `common::isolate()`.)
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs-fake-exec-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_write_script_writes_text_as_executable() {
    common::isolate();
    let dir = scratch_dir("writes");
    let script = dir.join("fake-tool");
    // Quotes, `$`, backslashes, `%` and the trailing newline reach the file unchanged.
    let text = "#!/bin/sh\n# it's \"$1\" \\ 100% \\n\necho \"ran $1\"\n";
    common::fake_exec::write_script(&script, text);
    assert_eq!(fs::read_to_string(&script).unwrap(), text);
    assert_eq!(fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755);
    let output = Command::new(&script).arg("x").output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran x\n");

    // Writing again replaces the script.
    common::fake_exec::write_script(&script, "#!/bin/sh\nexit 3\n");
    assert_eq!(Command::new(&script).status().unwrap().code(), Some(3));
    let _ = fs::remove_dir_all(&dir);
}

/// A script [`common::fake_exec::write_script`] wrote must start even while other threads of
/// the test process start processes. On Linux exec fails with ETXTBSY while any process holds
/// the file open for writing, and a child forked by another thread inherits this process's
/// descriptors until its own exec, so a script written through this process could fail to
/// start. Linux only: macOS runs a script that is open for writing, and spawning is slow enough
/// there that this test would take most of a minute.
#[cfg(target_os = "linux")]
#[test]
fn test_written_script_starts_while_other_threads_spawn_processes() {
    common::isolate();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let dir = scratch_dir("spawn_race");

    /// Stops and joins the spawner threads, also when the test panics, so they never keep
    /// forking while the binary's other tests run.
    struct Spawners {
        stop: Arc<AtomicBool>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }
    impl Drop for Spawners {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let spawners = Spawners {
        threads: (0..4)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("/bin/sh").args(["-c", ":"]).status();
                    }
                })
            })
            .collect(),
        stop,
    };

    // About 2 to 5 ms a round. With a helper that wrote the script through this process
    // (`fs::write`), 2 to 5 rounds in 1,000 failed in a Linux container, so 1,000 rounds catch it
    // on almost every run. The budget is far above the time that takes, so a slow runner still
    // runs every round; it only bounds a machine that has stalled.
    const ROUNDS: usize = 1000;
    const BUDGET: Duration = Duration::from_secs(60);
    let started = Instant::now();
    let mut rounds = 0;
    let mut failures = Vec::new();
    while rounds < ROUNDS && started.elapsed() < BUDGET {
        let script = dir.join(format!("fake-{}", rounds));
        common::fake_exec::write_script(&script, "#!/bin/sh\nexit 0\n");
        match Command::new(&script).status() {
            Ok(status) if status.success() => {}
            other => failures.push(format!("script {}: {:?}", rounds, other)),
        }
        rounds += 1;
    }
    drop(spawners);
    eprintln!("{} rounds in {:?}", rounds, started.elapsed());
    assert!(
        failures.is_empty(),
        "fake executable failed to start {} of {} times: {:?}",
        failures.len(),
        rounds,
        failures
    );
    let _ = fs::remove_dir_all(&dir);
}
