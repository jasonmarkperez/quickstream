//! Switching output to Local Audio resumes at the captured position (issue #44).
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use quickstream::player::local::{LocalPlayer, MpvLoadEnd, MpvLoadTracker};
use quickstream::player::AudioPlayer;
use serde_json::json;

/// 90s of silence (mp3, like the library), so the test never makes audible noise.
fn silent_track() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("quickstream_test_silence_90s.mp3");
    // Written under a temporary name and renamed, so an interrupted run never leaves a
    // truncated file that later runs would reuse.
    let partial = std::env::temp_dir().join(format!("quickstream_test_silence_90s.{}.mp3", std::process::id()));
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
            .arg(&partial)
            .status()
            .ok()?;
        if !status.success() {
            let _ = std::fs::remove_file(&partial);
            return None;
        }
        std::fs::rename(&partial, &path).ok()?;
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
                let mut range_start: Option<u64> = None;
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let l = line.trim().to_ascii_lowercase();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(v) = l.strip_prefix("range: bytes=") {
                        range_start = v.split('-').next().and_then(|n| n.parse().ok());
                    }
                    line.clear();
                }
                std::thread::sleep(delay);
                let data = std::fs::read(&file).unwrap();
                let total = data.len() as u64;
                let head = match range_start {
                    Some(start) if start >= total => format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        total
                    ),
                    Some(start) => format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        start,
                        total - 1,
                        total,
                        total - start
                    ),
                    None => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        total
                    ),
                };
                let body = match range_start {
                    Some(start) if start >= total => &data[..0],
                    Some(start) => &data[start as usize..],
                    None => &data[..],
                };
                let mut out = stream;
                let _ = out.write_all(head.as_bytes());
                let _ = out.write_all(body);
            });
        }
    });
    format!("http://127.0.0.1:{}/track.mp3", port)
}

/// Position as reported by mpv itself. `play_at` sets the position optimistically, so wait until
/// mpv has loaded the file (it reports a duration) and has played for a moment; otherwise the
/// optimistic value would be read back and the test could not tell where mpv really started.
fn mpv_reported_position(player: &LocalPlayer) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.duration_seconds() <= 0.0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(player.duration_seconds() > 0.0, "mpv never reported a duration");
    std::thread::sleep(Duration::from_millis(1200));
    player.position_seconds()
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
    let pos = mpv_reported_position(&player);
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
    let pos = mpv_reported_position(&player);
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
/// `commands.log` next to itself, and replies success. Lets the test see exactly what
/// LocalPlayer asks mpv to do.
fn recording_fake_mpv(dir: &std::path::Path) -> PathBuf {
    let script = dir.join("fake-mpv");
    let body = r#"#!/usr/bin/env python3
# (no extra shebang arguments: Linux passes "python3 -I" to env as one word)
import json, os, socket, sys
sock = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock)
srv.listen(4)
log = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "commands.log"), "a", buffering=1)
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
            reply = {"error": "success"}
            if "request_id" in req:
                reply["request_id"] = req["request_id"]
            conn.sendall((json.dumps(reply) + "\n").encode())
"#;
    common::fake_exec::write_script(&script, body);
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
    let fake = recording_fake_mpv(&dir);
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

/// Waits up to 5 s for `player` to report that its track finished.
fn wait_for_finished(player: &mut LocalPlayer) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if player.is_track_finished() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn test_mpv_play_at_past_the_end_finishes_the_track() {
    common::isolate();
    if !has("mpv") {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    let Some(track) = silent_track() else {
        eprintln!("SKIPPED: ffmpeg not installed");
        return;
    };

    // A tag length can be an estimate longer than the file, so a hand-off can ask for a start
    // past the real end. mpv then ends the load with reason "error"; the track must still count
    // as finished so the queue moves on instead of sitting stopped (issue #48).
    let mut player = LocalPlayer::new("mpv");
    player.play_at(&track, 95.0).expect("play_at");
    assert!(
        wait_for_finished(&mut player),
        "play_at past the end never reported the track finished"
    );
    assert!(!player.is_playing(), "nothing plays after a start past the end");
}

/// Waits up to 5 s for `player` to report an error, checking on every poll that it never
/// reports the track finished meanwhile. Returns the error, if one came.
fn wait_for_error_never_finishing(player: &mut LocalPlayer) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        assert!(
            !player.is_track_finished(),
            "a load that failed must not count as the track finishing"
        );
        if let Some(err) = player.take_error() {
            return Some(err);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn test_mpv_missing_file_started_past_zero_reports_an_error() {
    common::isolate();
    if !has("mpv") {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    // A share that went away mid-session: mpv ends the load with reason "error" (file_error
    // "loading failed"). Only a start past the file's real end counts as finished.
    let missing = std::env::temp_dir().join(format!("qs_missing_{}/gone.mp3", std::process::id()));
    let mut player = LocalPlayer::new("mpv");
    player.play_at(&missing, 42.0).expect("play_at");
    let err = wait_for_error_never_finishing(&mut player).expect("the failed load reports an error");
    assert!(
        err.contains("gone.mp3") && err.contains("loading failed"),
        "error: {err}"
    );
    assert!(!player.is_playing());
}

#[test]
fn test_mpv_corrupt_file_started_past_zero_reports_an_error() {
    common::isolate();
    if !has("mpv") {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("qs_corrupt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let corrupt = dir.join("corrupt.mp3");
    std::fs::write(
        &corrupt,
        b"not really audio, not really audio, not really audio".repeat(100),
    )
    .unwrap();

    let mut player = LocalPlayer::new("mpv");
    player.play_at(&corrupt, 42.0).expect("play_at");
    let err = wait_for_error_never_finishing(&mut player).expect("the failed load reports an error");
    assert!(err.contains("corrupt.mp3"), "error: {err}");
    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stand-in mpv that answers `loadfile` with the new playlist entry's id, as mpv does, and
/// then reports, before the new entry's `start-file`, that the entry it replaced failed: a late
/// `end-file` from a load that is no longer current. Writes `loadfile <n>` to `events.log` once
/// it has sent the events for load `n`.
fn late_end_file_fake_mpv(dir: &std::path::Path) -> PathBuf {
    let script = dir.join("fake-mpv");
    let body = r#"#!/usr/bin/env python3
import json, os, socket, sys
sock = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock)
srv.listen(4)
log = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "events.log"), "a", buffering=1)
entry = 0
def send(conn, msg):
    conn.sendall((json.dumps(msg) + "\n").encode())
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
            req = json.loads(line)
            reply = {"error": "success"}
            if "request_id" in req:
                reply["request_id"] = req["request_id"]
            if req.get("command", [None])[0] == "loadfile":
                entry += 1
                reply["data"] = {"playlist_entry_id": entry}
                send(conn, reply)
                if entry > 1:
                    send(conn, {"event": "end-file", "reason": "error", "playlist_entry_id": entry - 1,
                                "file_error": "loading failed"})
                send(conn, {"event": "start-file", "playlist_entry_id": entry})
                log.write("loadfile %d\n" % entry)
            else:
                send(conn, reply)
"#;
    common::fake_exec::write_script(&script, body);
    script
}

#[test]
fn test_mpv_end_of_a_replaced_load_is_not_the_new_loads_end() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("qs_fake_mpv_late_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = late_end_file_fake_mpv(&dir);
    let track = dir.join("track.mp3");
    std::fs::write(&track, b"not audio").unwrap();
    let events = dir.join("events.log");
    let sent = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&events)
                .unwrap_or_default()
                .contains(&format!("loadfile {n}"))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    };

    let mut player = LocalPlayer::with_mpv_binary("mpv", &fake);
    player.play(&track).expect("play");
    assert!(sent(1), "the fake mpv never saw the first load");
    // The second load starts past 0:00. The first load's late "error" end must neither count
    // as the second one finishing (a start past the end) nor be reported as its error.
    player.play_at(&track, 30.0).expect("play_at");
    assert!(sent(2), "the fake mpv never saw the second load");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !player.is_track_finished(),
        "a replaced load's end was taken for the new load's"
    );
    assert_eq!(player.take_error(), None, "a replaced load's error was reported");
    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// MpvLoadTracker fed mpv's messages directly, as each mpv generation sends them
// ---------------------------------------------------------------------------------------------

/// Feeds `msgs` to `tracker` and returns the end it reported, if any.
fn feed(tracker: &mut MpvLoadTracker, msgs: &[serde_json::Value]) -> Option<MpvLoadEnd> {
    msgs.iter().filter_map(|m| tracker.on_message(m)).last()
}

#[test]
fn test_mpv_tracker_with_the_entry_in_the_answer_ignores_the_replaced_entry() {
    common::isolate();
    let mut tracker = MpvLoadTracker::default();
    let id = tracker.begin(std::path::Path::new("/m/a.mp3"), true);
    let end = feed(
        &mut tracker,
        &[
            json!({"request_id": id, "error": "success", "data": {"playlist_entry_id": 2}}),
            json!({"event": "end-file", "reason": "error", "playlist_entry_id": 1, "file_error": "loading failed"}),
            json!({"event": "start-file", "playlist_entry_id": 2}),
            json!({"event": "file-loaded"}),
        ],
    );
    assert_eq!(end, None, "the replaced entry's end is not the new load's");
    let end = feed(
        &mut tracker,
        &[
            json!({"event": "end-file", "reason": "error", "playlist_entry_id": 2, "file_error": "no audio or video data played"}),
        ],
    );
    assert_eq!(
        end,
        Some(MpvLoadEnd::Finished),
        "a start past the end finishes the track"
    );
}

#[test]
fn test_mpv_tracker_without_the_entry_in_the_answer_takes_it_from_start_file() {
    common::isolate();
    // mpv 0.33 to 0.37: the events carry playlist_entry_id, loadfile's answer doesn't.
    let mut tracker = MpvLoadTracker::default();
    let id = tracker.begin(std::path::Path::new("/m/a.mp3"), false);
    let end = feed(
        &mut tracker,
        &[
            json!({"request_id": id, "error": "success"}),
            json!({"event": "end-file", "reason": "eof", "playlist_entry_id": 1}),
            json!({"event": "start-file", "playlist_entry_id": 2}),
            json!({"event": "file-loaded"}),
        ],
    );
    assert_eq!(end, None, "the replaced entry's end is not the new load's");
    assert!(tracker.is_current());
    let end = feed(
        &mut tracker,
        &[json!({"event": "end-file", "reason": "eof", "playlist_entry_id": 2})],
    );
    assert_eq!(end, Some(MpvLoadEnd::Finished));
}

#[test]
fn test_mpv_tracker_without_any_entry_ids_still_sees_the_track_end() {
    common::isolate();
    // mpv older than 0.33 names no playlist entries at all: after loadfile's answer and the
    // next start-file, the events are the newest load's, so a track still ends.
    let mut tracker = MpvLoadTracker::default();
    let id = tracker.begin(std::path::Path::new("/m/a.mp3"), false);
    let end = feed(
        &mut tracker,
        &[
            json!({"request_id": id, "error": "success"}),
            json!({"event": "end-file", "reason": "eof"}),
            json!({"event": "start-file"}),
            json!({"event": "file-loaded"}),
        ],
    );
    assert_eq!(end, None, "the end before the new start-file is the replaced load's");
    assert!(tracker.is_current(), "eof-reached counts once the new load has started");
    let end = feed(&mut tracker, &[json!({"event": "end-file", "reason": "eof"})]);
    assert_eq!(end, Some(MpvLoadEnd::Finished), "the track's own end is seen");
}

#[test]
fn test_mpv_tracker_without_any_entry_ids_still_reports_a_failed_load() {
    common::isolate();
    let mut tracker = MpvLoadTracker::default();
    let id = tracker.begin(std::path::Path::new("/m/gone.mp3"), true);
    let end = feed(
        &mut tracker,
        &[
            json!({"request_id": id, "error": "success"}),
            json!({"event": "start-file"}),
            json!({"event": "end-file", "reason": "error", "file_error": "loading failed"}),
        ],
    );
    assert_eq!(
        end,
        Some(MpvLoadEnd::Failed(
            "couldn't play gone.mp3 (loading failed)".to_string()
        ))
    );
}
