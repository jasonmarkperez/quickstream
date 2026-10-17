//! LocalPlayer hardening: private mpv IPC socket directory and child process reaping (#10).
//!
//! Every test that plays audio uses silent audio generated into a scratch directory.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use quickstream::player::local::{create_private_ipc_dir, verify_ipc_socket, LocalPlayer};
use quickstream::player::AudioPlayer;

/// Fresh scratch dir unique to this process and test. Also points the XDG config/cache/data
/// dirs at it so nothing (e.g. the logger) touches the real user directories.
fn scratch_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("qs-hard-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for var in ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"] {
        std::env::set_var(var, root.join(var.to_lowercase()));
    }
    let dir = root.join(name);
    fs::create_dir(&dir).expect("scratch dir must not already exist");
    dir
}

/// Remove a scratch dir created by `scratch_dir` (refuses anything else).
fn cleanup(dir: &Path) {
    let root = std::env::temp_dir().join(format!("qs-hard-{}", std::process::id()));
    assert!(dir.starts_with(&root), "refusing to remove {:?}", dir);
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

/// True while the pid exists at all, including as an unreaped zombie.
fn process_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[test]
fn test_private_ipc_dir_is_0700_and_owned_by_current_user() {
    let base = scratch_dir("dir-perms");
    let dir = base.join("ipc");

    create_private_ipc_dir(&dir).expect("fresh private dir should be created");

    let meta = fs::symlink_metadata(&dir).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700, "ipc dir must be mode 0700");
    assert_eq!(meta.uid(), current_uid(), "ipc dir must be owned by the current user");

    cleanup(&base);
}

#[test]
fn test_private_ipc_dir_refuses_existing_directory() {
    let base = scratch_dir("dir-exists");
    let dir = base.join("ipc");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

    assert!(create_private_ipc_dir(&dir).is_err(), "pre-existing dir must be refused");
    // Must not have been "fixed up" and adopted either.
    let mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o777);

    cleanup(&base);
}

#[test]
fn test_private_ipc_dir_refuses_symlink() {
    let base = scratch_dir("dir-symlink");
    let target = base.join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let link = base.join("ipc");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    assert!(create_private_ipc_dir(&link).is_err(), "symlinked dir must be refused");
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "symlink target must be left untouched");

    cleanup(&base);
}

#[test]
fn test_verify_ipc_socket_checks_type_and_owner() {
    let base = scratch_dir("verify-sock");
    let sock = base.join("s.sock");
    let _listener = UnixListener::bind(&sock).unwrap();
    assert!(verify_ipc_socket(&sock).is_ok(), "own socket should be accepted");

    let regular = base.join("regular");
    fs::write(&regular, b"x").unwrap();
    assert!(verify_ipc_socket(&regular).is_err(), "regular file must be rejected");

    let link = base.join("link.sock");
    std::os::unix::fs::symlink(&sock, &link).unwrap();
    assert!(verify_ipc_socket(&link).is_err(), "symlink to a socket must be rejected");

    assert!(verify_ipc_socket(&base.join("missing.sock")).is_err());

    drop(_listener);
    cleanup(&base);
}

#[test]
fn test_mpv_socket_lives_in_private_runtime_dir_and_is_removed_on_drop() {
    if !have("mpv") || !have("ffmpeg") {
        eprintln!("skipping: mpv/ffmpeg not installed");
        return;
    }
    // Kept short: unix socket paths are limited to ~100 bytes.
    let base = scratch_dir("rt");
    let audio = silent_wav(&base, "silence.wav", 5);
    let runtime = base.clone();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();

    let prev = std::env::var_os("XDG_RUNTIME_DIR");
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    let mut player = LocalPlayer::new("mpv");
    let res = player.play(&audio);
    match prev {
        Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
        None => std::env::remove_var("XDG_RUNTIME_DIR"),
    }
    res.expect("play should succeed");

    let sock = player.ipc_socket_path().expect("mpv backend should have an ipc socket").to_path_buf();
    let dir = sock.parent().unwrap().to_path_buf();
    assert_eq!(dir.parent().unwrap(), runtime.as_path(), "socket dir should live under XDG_RUNTIME_DIR");
    let meta = fs::symlink_metadata(&dir).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), current_uid());
    assert!(verify_ipc_socket(&sock).is_ok(), "socket should be ours");

    let _ = player.stop();
    drop(player);
    assert!(!sock.exists(), "socket should be removed on drop");
    assert!(!dir.exists(), "private socket dir should be removed on drop");

    cleanup(&base);
}

#[test]
fn test_mpv_socket_dir_name_is_unpredictable() {
    if !have("mpv") || !have("ffmpeg") {
        eprintln!("skipping: mpv/ffmpeg not installed");
        return;
    }
    let base = scratch_dir("unpredictable");
    let audio = silent_wav(&base, "silence.wav", 5);

    let mut player = LocalPlayer::new("mpv");
    player.play(&audio).expect("play should succeed");
    let sock = player.ipc_socket_path().unwrap().to_path_buf();
    let dir = sock.parent().unwrap();
    assert_ne!(dir, Path::new("/tmp"), "socket must not sit directly in shared /tmp: {:?}", sock);
    assert_ne!(dir, std::env::temp_dir().as_path(), "socket must be in its own dir: {:?}", sock);
    let pid = std::process::id().to_string();
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(!dir_name.contains(&pid), "socket dir name must not be derived from the pid: {}", dir_name);
    assert!(dir_name.len() >= 16, "socket dir name should carry a random component: {}", dir_name);

    // A second player gets a different, unrelated directory.
    let mut other = LocalPlayer::new("mpv");
    other.play(&audio).expect("second play should succeed");
    let other_dir = other.ipc_socket_path().unwrap().parent().unwrap().to_path_buf();
    assert_ne!(other_dir, dir);
    let _ = other.stop();
    drop(other);

    let _ = player.stop();
    drop(player);
    cleanup(&base);
}

#[test]
fn test_failed_mpv_connect_kills_and_reaps_spawned_child() {
    if !have("ffmpeg") || !have("ffplay") {
        eprintln!("skipping: ffmpeg/ffplay not installed");
        return;
    }
    let base = scratch_dir("fake-mpv");
    let audio = silent_wav(&base, "silence.wav", 5);

    // A fake mpv that never creates the IPC socket and would otherwise linger for 60s.
    let pid_file = base.join("pids");
    let fake = base.join("fake-mpv");
    fs::write(
        &fake,
        format!("#!/bin/sh\necho $$ >> '{}'\nexec sleep 60\n", pid_file.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let mut player = LocalPlayer::with_mpv_binary("mpv", &fake);
    // Each attempt fails to connect and falls back to (silent) ffplay playback.
    assert!(player.play(&audio).is_ok());
    assert!(player.play(&audio).is_ok());

    let pids: Vec<i32> = fs::read_to_string(&pid_file)
        .expect("fake mpv should have been spawned")
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(pids.len(), 2, "one fake mpv per play attempt");

    let leaked: Vec<i32> = pids.iter().copied().filter(|&p| process_exists(p)).collect();
    // Clean up regardless of outcome so a failing run doesn't leave processes behind,
    // but only signal processes that are verifiably our fake mpv (`sleep 60`).
    for &p in &leaked {
        let cmdline = Command::new("ps")
            .args(["-o", "command=", "-p", &p.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if cmdline == "sleep 60" {
            unsafe {
                libc::kill(p, libc::SIGKILL);
            }
        }
    }
    let _ = player.stop();
    drop(player);
    cleanup(&base);

    assert!(leaked.is_empty(), "fake mpv children leaked (running or zombie): {:?}", leaked);
}
