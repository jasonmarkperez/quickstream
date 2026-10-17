//! LocalPlayer duration handling (#14): no stale duration after a track change, and the
//! ffplay fallback reports a duration (so position and seeks are bounded).
//!
//! Every test that plays audio uses silent audio generated into a scratch directory.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use quickstream::player::local::LocalPlayer;
use quickstream::player::AudioPlayer;

fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(format!("qs-dur-{}", std::process::id()))
}

/// Fresh scratch dir unique to this process and test. (The XDG dirs are isolated by
/// `common::isolate()`.)
fn scratch_dir(name: &str) -> PathBuf {
    let root = scratch_root();
    fs::create_dir_all(&root).unwrap();
    let dir = root.join(name);
    fs::create_dir(&dir).expect("scratch dir must not already exist");
    dir
}

/// Remove a scratch dir created by `scratch_dir` (refuses anything else).
fn cleanup(dir: &Path) {
    assert!(dir.starts_with(scratch_root()), "refusing to remove {:?}", dir);
    let _ = fs::remove_dir_all(dir);
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", bin))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn silent_wav(dir: &Path, name: &str, secs: u32) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t"])
        .arg(secs.to_string())
        .arg(&path)
        .status()
        .expect("ffmpeg must run");
    assert!(status.success(), "ffmpeg failed to generate silent audio");
    path
}

#[test]
fn test_ffplay_duration_comes_from_metadata_and_bounds_seek() {
    common::isolate();
    if !have("ffmpeg") || !have("ffplay") {
        eprintln!("skipping: ffmpeg/ffplay not installed");
        return;
    }
    let base = scratch_dir("ffplay-dur");
    let a = silent_wav(&base, "a.wav", 4);
    let b = silent_wav(&base, "b.wav", 2);

    let mut player = LocalPlayer::new("ffplay");
    player.play(&a).unwrap();
    let dur_a = player.duration_seconds();
    assert!((dur_a - 4.0).abs() < 0.3, "ffplay duration should be ~4s, got {:.3}", dur_a);

    player.pause().unwrap();
    player.seek_to(1000.0).unwrap();
    let pos = player.position_seconds();
    assert!(pos <= dur_a + 0.01, "seek must be clamped to duration, got {:.3}", pos);

    player.play(&b).unwrap();
    let dur_b = player.duration_seconds();
    assert!((dur_b - 2.0).abs() < 0.3, "duration should follow the new track, got {:.3}", dur_b);

    let _ = player.stop();
    drop(player);
    cleanup(&base);
}

#[test]
fn test_mpv_play_resets_stale_duration() {
    common::isolate();
    if !have("mpv") || !have("ffmpeg") {
        eprintln!("skipping: mpv/ffmpeg not installed");
        return;
    }
    let base = scratch_dir("mpv-dur");
    let a = silent_wav(&base, "a.wav", 7);
    let b = silent_wav(&base, "b.wav", 3);

    let mut player = LocalPlayer::new("mpv");
    player.play(&a).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while (player.duration_seconds() - 7.0).abs() > 0.3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!((player.duration_seconds() - 7.0).abs() <= 0.3, "mpv should report the first track's duration");

    player.play(&b).unwrap();
    let d = player.duration_seconds();
    assert!(
        (d - 7.0).abs() > 0.5,
        "duration right after switching tracks must not be the previous track's ({:.3})",
        d
    );

    let _ = player.stop();
    drop(player);
    cleanup(&base);
}
