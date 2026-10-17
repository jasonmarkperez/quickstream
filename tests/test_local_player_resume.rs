//! Switching output to Local Audio resumes at the captured position (issue #44).
mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use quickstream::player::local::LocalPlayer;
use quickstream::player::AudioPlayer;

/// 90s of silence (mp3, like the library), so the test never makes audible noise.
fn silent_track() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("quickstream_test_silence_90s.mp3");
    if !path.exists() {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=stereo",
                "-t",
                "90",
            ])
            .arg(&path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
    }
    Some(path)
}

fn has(binary: &str) -> bool {
    std::process::Command::new(binary).arg("-version").output().is_ok()
}

/// Serves `file` over HTTP on 127.0.0.1 with Range support, waiting `delay` before every
/// response — like opening a file on a slow network share.
fn slow_http_server(file: PathBuf, delay: Duration) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let file = file.clone();
            std::thread::spawn(move || {
                let _ = stream.set_nonblocking(false);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range_start = 0u64;
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let l = line.trim().to_ascii_lowercase();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(v) = l.strip_prefix("range: bytes=") {
                        range_start = v.split('-').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                    }
                    line.clear();
                }
                std::thread::sleep(delay);
                let data = std::fs::read(&file).unwrap();
                let total = data.len() as u64;
                let start = range_start.min(total);
                let body = &data[start as usize..];
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    start,
                    total.saturating_sub(1),
                    total,
                    body.len()
                );
                let mut out = stream;
                let _ = out.write_all(head.as_bytes());
                let _ = out.write_all(body);
            });
        }
    });
    format!("http://127.0.0.1:{}/track.mp3", port)
}

fn wait_for_position(player: &LocalPlayer, min: f64) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut pos = player.position_seconds();
    while pos < min && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        pos = player.position_seconds();
    }
    pos
}

#[test]
fn test_mpv_play_at_starts_at_requested_position() {
    common::isolate();
    if !has("mpv") {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    let Some(track) = silent_track() else {
        eprintln!("SKIPPED: ffmpeg not installed");
        return;
    };

    let mut player = LocalPlayer::new("mpv");
    player.play_at(&track, 42.0).expect("play_at");
    let pos = wait_for_position(&player, 41.0);
    assert!(
        (41.0..50.0).contains(&pos),
        "mpv started at {:.2}s instead of ~42s",
        pos
    );

    // The start position must not carry over to the next track.
    player.play(&track).expect("play");
    std::thread::sleep(Duration::from_millis(800));
    let pos = player.position_seconds();
    assert!(pos < 5.0, "next play() started at {:.2}s instead of 0", pos);
}

#[test]
fn test_mpv_play_at_on_slow_source_starts_at_requested_position() {
    common::isolate();
    if !has("mpv") {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    let Some(track) = silent_track() else {
        eprintln!("SKIPPED: ffmpeg not installed");
        return;
    };
    // Opening takes a while (like /Volumes/Music over SMB), so a seek sent right after
    // `loadfile` arrives before the file is loaded.
    let url = slow_http_server(track, Duration::from_millis(700));

    let mut player = LocalPlayer::new("mpv");
    player.play_at(std::path::Path::new(&url), 42.0).expect("play_at");
    let pos = wait_for_position(&player, 41.0);
    assert!(
        (41.0..50.0).contains(&pos),
        "mpv started at {:.2}s instead of ~42s",
        pos
    );
}

#[test]
fn test_ffplay_play_at_reports_requested_position() {
    common::isolate();
    if !has("ffplay") {
        eprintln!("SKIPPED: ffplay not installed");
        return;
    }
    let Some(track) = silent_track() else {
        eprintln!("SKIPPED: ffmpeg not installed");
        return;
    };

    let mut player = LocalPlayer::new("ffplay");
    player.play_at(&track, 42.0).expect("play_at");
    let pos = player.position_seconds();
    assert!(
        (42.0..45.0).contains(&pos),
        "ffplay position {:.2}s instead of ~42s",
        pos
    );
    player.stop().unwrap();
}

/// A stand-in mpv that creates the IPC socket, records every JSON command it receives to
/// `log`, and replies success. Lets the test see exactly what LocalPlayer asks mpv to do.
fn recording_fake_mpv(dir: &std::path::Path, log: &std::path::Path) -> PathBuf {
    let script = dir.join("fake-mpv");
    let body = format!(
        r#"#!/usr/bin/env python3 -I
import json, os, socket, sys
sock = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock)
srv.listen(4)
log = open({log:?}, "a", buffering=1)
while True:
    conn, _ = srv.accept()
    buf = b""
    while True:
        data = conn.recv(65536)
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            log.write(line.decode() + "\n")
            req = json.loads(line)
            reply = {{"error": "success"}}
            if "request_id" in req:
                reply["request_id"] = req["request_id"]
            conn.sendall((json.dumps(reply) + "\n").encode())
"#,
        log = log.to_string_lossy()
    );
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn recorded_commands(log: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| v["command"].clone())
        .collect()
}

fn wait_for_commands(log: &std::path::Path, pred: impl Fn(&[serde_json::Value]) -> bool) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let cmds = recorded_commands(log);
        if pred(&cmds) || Instant::now() > deadline {
            return cmds;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn test_mpv_play_at_loads_file_with_start_position() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("qs_fake_mpv_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("commands.log");
    let fake = recording_fake_mpv(&dir, &log);
    let track = dir.join("track.mp3");
    std::fs::write(&track, b"not audio").unwrap();

    let mut player = LocalPlayer::with_mpv_binary("mpv", &fake);
    player.play_at(&track, 42.0).expect("play_at");
    let cmds = wait_for_commands(&log, |c| c.iter().any(|c| c[0] == "loadfile"));

    // On a slow share mpv rejects a seek sent before the file is loaded, so the start position
    // must be set before `loadfile` instead of seeking afterwards.
    let load = cmds.iter().position(|c| c[0] == "loadfile").expect("loadfile sent");
    let start = cmds
        .iter()
        .position(|c| c[0] == "set_property" && c[1] == "start")
        .expect("start position set");
    assert!(start < load, "start must be set before loadfile: {:?}", cmds);
    assert_eq!(cmds[start][2], "42.000", "{:?}", cmds);
    assert!(
        !cmds.iter().any(|c| c[0] == "seek"),
        "no seek right after loadfile: {:?}",
        cmds
    );

    // A later plain play() must start from the beginning.
    let before = cmds.len();
    player.play(&track).expect("play");
    let cmds = wait_for_commands(&log, |c| c[before..].iter().any(|c| c[0] == "loadfile"));
    let next = &cmds[before..];
    let load = next.iter().position(|c| c[0] == "loadfile").expect("second loadfile");
    let reset = next
        .iter()
        .position(|c| c[0] == "set_property" && c[1] == "start")
        .expect("start reset");
    assert!(reset < load && next[reset][2] == "none", "{:?}", next);

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}
